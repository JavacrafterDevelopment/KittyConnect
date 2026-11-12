package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/netip"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"github.com/tailscale/tailcat"
	"tailscale.com/tailcfg"
	"tailscale.com/types/key"
	"tailscale.com/types/logger"
	"tailscale.com/wgengine/filter"
)

// loopback is the only address this helper ever listens on or connects to
// outside the tunnel.
var loopback = netip.MustParseAddr("127.0.0.1")

// udpBuffer is the socket buffer size asked for on the local UDP sockets, so a
// burst of screen-video pieces isn't dropped before it's read.
const udpBuffer = 4 << 20

// flowIdle is how long a forwarded UDP flow lives without traffic. KittyConnect
// sends a keepalive every few seconds, so only a gone member reaches it.
const flowIdle = 3 * time.Minute

// serve starts a TailCat server for one port, TCP and UDP, proxied to
// 127.0.0.1:port, and prints its address the way the official CLI does.
//
// region is nil in normal use: the nearest TailCat relay region is picked, and
// the address names it by ID (the short form, as the official CLI makes).
// Tests pass a local relay, which is then embedded in the address.
func serve(ctx context.Context, logf logger.Logf, port uint16, region *tailcfg.DERPRegion) (*tailcat.Server, tailcat.Addr, error) {
	embed := region != nil
	if region == nil {
		ci := &tailcat.ConnInfo{RegionID: -1} // pick by latency
		ectx, cancel := context.WithTimeout(ctx, 30*time.Second)
		err := ci.Expand(ectx, tailcat.ExpandForServer)
		cancel()
		if err != nil {
			return nil, "", fmt.Errorf("couldn't pick a TailCat relay: %w", err)
		}
		region = ci.Region[0]
		clearUnnecessaryRegionFields(region)
		fmt.Fprintf(os.Stderr, "# Selected bootstrap relay region %v, %v\n", region.RegionID, region.RegionName)
	}

	// A fresh identity and pre-shared key for every room.
	priv := key.NewNode()
	psk := tailcat.NewPresharedKey()
	if psk.IsZero() {
		return nil, "", errors.New("refusing to serve without a WireGuard pre-shared key")
	}

	only := []filter.PortRange{{First: port, Last: port}}
	s := &tailcat.Server{
		Key:          priv,
		PresharedKey: psk,
		Logf:         logf,
		Region:       region,
		// The packet filter admits only this port, TCP and UDP, and the
		// handlers below check it again.
		ServedTCPPorts: only,
		ServedUDPPorts: only,
		UDPIdleTimeout: flowIdle,
	}
	local := netip.AddrPortFrom(loopback, port)
	s.OnTCP = func(p uint16) func(net.Conn) {
		if p != port {
			return nil
		}
		return func(c net.Conn) {
			lc, err := net.DialTCP("tcp4", nil, net.TCPAddrFromAddrPort(local))
			if err != nil {
				logf("error proxying to %v: %v", local, err)
				c.Close()
				return
			}
			tailcat.ProxyConns(c, lc)
		}
	}
	s.OnUDP = func(p uint16) func(tailcat.ConnPacketConn) {
		if p != port {
			return nil
		}
		return func(c tailcat.ConnPacketConn) {
			// One local socket per member's flow, so the room tells
			// members apart by the address their packets come from.
			lc, err := net.DialUDP("udp4", nil, net.UDPAddrFromAddrPort(local))
			if err != nil {
				logf("error proxying UDP to %v: %v", local, err)
				c.Close()
				return
			}
			lc.SetReadBuffer(udpBuffer)
			lc.SetWriteBuffer(udpBuffer)
			proxyFlow(c, lc)
		}
	}
	if err := s.Start(); err != nil {
		return nil, "", fmt.Errorf("couldn't start the TailCat server: %w", err)
	}

	ci := tailcat.ConnInfo{
		ServerPublic:      tailcat.NodePublic{NodePublic: priv.Public()},
		ServerDiscoPublic: tailcat.DiscoPublicForNode(priv),
		PresharedKey:      psk,
	}
	if embed {
		ci.Region = []*tailcfg.DERPRegion{region}
	} else {
		ci.RegionID = region.RegionID
	}
	addr := ci.Addr()
	fmt.Fprintf(os.Stderr, "# 🐈 Server listening with new address: %v\n", addr)
	return s, addr, nil
}

// proxyFlow copies datagrams between one tunnel flow and the local socket
// connected to the room, until the flow ends (the member goes quiet past
// flowIdle, or leaves).
func proxyFlow(tunnel tailcat.ConnPacketConn, local *net.UDPConn) {
	done := make(chan struct{})
	go func() {
		defer close(done)
		buf := make([]byte, 65535)
		for {
			n, err := local.Read(buf)
			if err != nil {
				if errors.Is(err, net.ErrClosed) {
					return
				}
				// On Windows a connected UDP socket reports an
				// earlier ICMP "port unreachable" as a read error.
				// The socket is still fine.
				continue
			}
			if _, err := tunnel.Write(buf[:n]); err != nil {
				return
			}
		}
	}()
	buf := make([]byte, 65535)
	for {
		n, err := tunnel.Read(buf)
		if err != nil {
			break
		}
		local.Write(buf[:n])
	}
	tunnel.Close()
	local.Close()
	<-done
}

// forwarder listens on 127.0.0.1:<local> for TCP and UDP and carries both to
// the server's port through the tunnel.
type forwarder struct {
	cl     *tailcat.Client
	remote uint16
	logf   logger.Logf
	tcp    *net.TCPListener
	udp    *net.UDPConn
	ctx    context.Context
	cancel context.CancelFunc

	mu    sync.Mutex
	flows map[netip.AddrPort]*udpFlow
	conns map[net.Conn]struct{}
}

// udpFlow is one local UDP socket's traffic, carried as one tunnel flow.
type udpFlow struct {
	ready chan struct{} // closed once conn is set, or the dial failed
	conn  tailcat.ConnPacketConn
	last  atomic.Int64 // unix nanos of the last packet either way
	dead  atomic.Bool
}

func forward(ctx context.Context, logf logger.Logf, addr tailcat.Addr, local, remote uint16) (*forwarder, error) {
	at := netip.AddrPortFrom(loopback, local)
	tl, err := net.ListenTCP("tcp4", net.TCPAddrFromAddrPort(at))
	if err != nil {
		return nil, fmt.Errorf("listen on %v: %w", at, err)
	}
	ul, err := net.ListenUDP("udp4", net.UDPAddrFromAddrPort(at))
	if err != nil {
		tl.Close()
		return nil, fmt.Errorf("listen on udp %v: %w", at, err)
	}
	ul.SetReadBuffer(udpBuffer)
	ul.SetWriteBuffer(udpBuffer)
	ctx, cancel := context.WithCancel(ctx)
	f := &forwarder{
		cl:     &tailcat.Client{Server: addr, Logf: logf}, // a new ephemeral client key
		remote: remote,
		logf:   logf,
		tcp:    tl,
		udp:    ul,
		ctx:    ctx,
		cancel: cancel,
		flows:  map[netip.AddrPort]*udpFlow{},
		conns:  map[net.Conn]struct{}{},
	}
	// The same line the official CLI prints, then one for UDP.
	fmt.Fprintf(os.Stderr, "# forwarding %s -> remote localhost:%d\n", tl.Addr(), remote)
	fmt.Fprintf(os.Stderr, "# forwarding udp %s -> remote localhost:%d\n", ul.LocalAddr(), remote)
	go f.acceptTCP()
	go f.readUDP()
	go f.reapUDP()
	return f, nil
}

func (f *forwarder) Close() error {
	f.cancel()
	f.tcp.Close()
	f.udp.Close()
	f.mu.Lock()
	for _, fl := range f.flows {
		fl.close()
	}
	for c := range f.conns {
		c.Close()
	}
	f.mu.Unlock()
	return f.cl.Close()
}

func (f *forwarder) acceptTCP() {
	for {
		c, err := f.tcp.Accept()
		if err != nil {
			return
		}
		f.mu.Lock()
		f.conns[c] = struct{}{}
		f.mu.Unlock()
		go func() {
			defer func() {
				f.mu.Lock()
				delete(f.conns, c)
				f.mu.Unlock()
				c.Close()
			}()
			rc, err := f.cl.DialTCPPort(f.ctx, f.remote)
			if err != nil {
				if f.ctx.Err() == nil {
					f.logf("dial remote target localhost:%d: %v", f.remote, err)
				}
				return
			}
			tailcat.ProxyConns(c, rc)
		}()
	}
}

func (f *forwarder) readUDP() {
	buf := make([]byte, 65535)
	for {
		n, src, err := f.udp.ReadFromUDPAddrPort(buf)
		if err != nil {
			if errors.Is(err, net.ErrClosed) || f.ctx.Err() != nil {
				return
			}
			// Windows reports an ICMP "port unreachable" from an earlier
			// send (to a local socket that has closed) as a read error.
			continue
		}
		if src.Addr().Unmap() != loopback {
			continue // only this PC
		}
		fl := f.flowFor(src)
		select {
		case <-fl.ready:
			if fl.conn != nil && !fl.dead.Load() {
				fl.last.Store(time.Now().UnixNano())
				fl.conn.Write(buf[:n])
			}
		default:
			// Still connecting: the packet is dropped, as the network
			// would. KittyConnect retries what matters.
		}
	}
}

// flowFor returns the tunnel flow for packets from src, starting one if
// there's none (or the last one ended).
func (f *forwarder) flowFor(src netip.AddrPort) *udpFlow {
	f.mu.Lock()
	defer f.mu.Unlock()
	if fl := f.flows[src]; fl != nil && !fl.dead.Load() {
		return fl
	}
	fl := &udpFlow{ready: make(chan struct{})}
	fl.last.Store(time.Now().UnixNano())
	f.flows[src] = fl
	go func() {
		dctx, cancel := context.WithTimeout(f.ctx, 60*time.Second)
		c, err := f.cl.DialUDPPort(dctx, f.remote)
		cancel()
		if err != nil {
			if f.ctx.Err() == nil {
				f.logf("dial remote UDP localhost:%d: %v", f.remote, err)
			}
			fl.dead.Store(true)
			close(fl.ready)
			return
		}
		fl.conn = c
		close(fl.ready)
		buf := make([]byte, 65535)
		for {
			n, err := c.Read(buf)
			if err != nil {
				break
			}
			fl.last.Store(time.Now().UnixNano())
			f.udp.WriteToUDPAddrPort(buf[:n], src)
		}
		fl.close()
	}()
	return fl
}

func (fl *udpFlow) close() {
	fl.dead.Store(true)
	select {
	case <-fl.ready:
		if fl.conn != nil {
			fl.conn.Close()
		}
	default:
	}
}

// reapUDP ends flows nothing has used for flowIdle.
func (f *forwarder) reapUDP() {
	t := time.NewTicker(30 * time.Second)
	defer t.Stop()
	for {
		select {
		case <-f.ctx.Done():
			return
		case <-t.C:
		}
		cutoff := time.Now().Add(-flowIdle).UnixNano()
		f.mu.Lock()
		for src, fl := range f.flows {
			if fl.dead.Load() || fl.last.Load() < cutoff {
				fl.close()
				delete(f.flows, src)
			}
		}
		f.mu.Unlock()
	}
}

// clearUnnecessaryRegionFields shortens the relay details, as the official
// CLI does before it prints an address.
func clearUnnecessaryRegionFields(r *tailcfg.DERPRegion) {
	r.Latitude = 0
	r.Longitude = 0
	r.RegionCode = ""
	if len(r.Nodes) > 1 {
		r.Nodes = r.Nodes[:1]
	}
	for _, n := range r.Nodes {
		n.CanPort80 = false
		n.RegionID = 0
	}
}
