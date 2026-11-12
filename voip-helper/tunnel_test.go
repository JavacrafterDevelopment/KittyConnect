package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"strconv"
	"sync"
	"testing"
	"time"

	"github.com/tailscale/tailcat"
	"tailscale.com/derp/derpserver"
	"tailscale.com/net/stun"
	"tailscale.com/tailcfg"
	"tailscale.com/types/key"
	"tailscale.com/types/logger"
)

// localRelay runs a TailCat relay (DERP and STUN) on 127.0.0.1, as the
// official CLI's own tests do, so the tunnel can be tested without the
// internet.
func localRelay(t *testing.T) *tailcfg.DERPRegion {
	t.Helper()
	d := derpserver.New(key.NewNode(), logger.Discard)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	srv := httptest.NewUnstartedServer(derpserver.Handler(d))
	srv.Listener = ln
	srv.Config.TLSNextProto = make(map[string]func(*http.Server, *tls.Conn, http.Handler))
	srv.StartTLS()
	t.Cleanup(srv.Close)

	uln, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { uln.Close() })
	go func() {
		var buf [1500]byte
		for {
			n, src, err := uln.ReadFromUDPAddrPort(buf[:])
			if err != nil {
				return
			}
			if txid, err := stun.ParseBindingRequest(buf[:n]); err == nil {
				uln.WriteToUDPAddrPort(stun.Response(txid, src), src)
			}
		}
	}()
	return &tailcfg.DERPRegion{
		RegionID:   1,
		RegionCode: "D",
		Nodes: []*tailcfg.DERPNode{{
			Name:             "t1",
			RegionID:         1,
			HostName:         "T",
			IPv4:             "127.0.0.1",
			IPv6:             "none",
			STUNPort:         uln.LocalAddr().(*net.UDPAddr).Port,
			DERPPort:         ln.Addr().(*net.TCPAddr).Port,
			InsecureForTests: true,
		}},
	}
}

// echoRoom stands in for a KittyConnect room: TCP and UDP echo on the same
// 127.0.0.1 port. It records where each UDP datagram came from.
type echoRoom struct {
	port uint16
	mu   sync.Mutex
	srcs map[netip.AddrPort]bool
}

func newEchoRoom(t *testing.T) *echoRoom {
	t.Helper()
	for range 20 {
		tl, err := net.ListenTCP("tcp4", &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1)})
		if err != nil {
			t.Fatal(err)
		}
		port := uint16(tl.Addr().(*net.TCPAddr).Port)
		ul, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: int(port)})
		if err != nil {
			tl.Close()
			continue
		}
		t.Cleanup(func() { tl.Close(); ul.Close() })
		r := &echoRoom{port: port, srcs: map[netip.AddrPort]bool{}}
		go func() {
			for {
				c, err := tl.Accept()
				if err != nil {
					return
				}
				go func() { io.Copy(c, c); c.Close() }()
			}
		}()
		go func() {
			buf := make([]byte, 65535)
			for {
				n, src, err := ul.ReadFromUDPAddrPort(buf)
				if err != nil {
					return
				}
				r.mu.Lock()
				r.srcs[src] = true
				r.mu.Unlock()
				ul.WriteToUDPAddrPort(buf[:n], src)
			}
		}()
		return r
	}
	t.Fatal("no free port for TCP and UDP")
	return nil
}

func (r *echoRoom) sources() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return len(r.srcs)
}

func freePort(t *testing.T) uint16 {
	t.Helper()
	for range 20 {
		tl, err := net.ListenTCP("tcp4", &net.TCPAddr{IP: net.IPv4(127, 0, 0, 1)})
		if err != nil {
			t.Fatal(err)
		}
		port := tl.Addr().(*net.TCPAddr).Port
		ul, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: port})
		tl.Close()
		if err == nil {
			ul.Close()
			return uint16(port)
		}
	}
	t.Fatal("no free port")
	return 0
}

// udpEcho sends msg from c until it comes back (the first packets are dropped
// while the tunnel comes up, as KittyConnect's registration expects).
func udpEcho(t *testing.T, c *net.UDPConn, msg []byte) {
	t.Helper()
	deadline := time.Now().Add(45 * time.Second)
	buf := make([]byte, 65535)
	for time.Now().Before(deadline) {
		c.Write(msg)
		c.SetReadDeadline(time.Now().Add(300 * time.Millisecond))
		n, err := c.Read(buf)
		if err == nil {
			if !bytes.Equal(buf[:n], msg) {
				t.Fatalf("UDP echo: got %d bytes, want %d (boundaries must be kept)", n, len(msg))
			}
			return
		}
	}
	t.Fatalf("no UDP echo of %d bytes through the tunnel", len(msg))
}

// A room's TCP and UDP both cross the tunnel, datagram boundaries survive,
// and two members' UDP sockets reach the room as two different sources.
func TestTunnelCarriesTCPAndUDP(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	region := localRelay(t)
	room := newEchoRoom(t)

	s, addr, err := serve(ctx, t.Logf, room.port, region)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ci, err := tailcat.ParseAddr(addr)
	if err != nil {
		t.Fatal(err)
	}
	if ci.PresharedKey.IsZero() {
		t.Fatal("the address must carry a WireGuard pre-shared key")
	}

	local := freePort(t)
	f, err := forward(ctx, t.Logf, addr, local, room.port)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	if ip := f.tcp.Addr().(*net.TCPAddr).IP; !ip.Equal(net.IPv4(127, 0, 0, 1)) {
		t.Fatalf("TCP listens on %v, want 127.0.0.1 only", ip)
	}
	if ip := f.udp.LocalAddr().(*net.UDPAddr).IP; !ip.Equal(net.IPv4(127, 0, 0, 1)) {
		t.Fatalf("UDP listens on %v, want 127.0.0.1 only", ip)
	}

	// TCP: chat, names, attachments.
	var tc net.Conn
	deadline := time.Now().Add(45 * time.Second)
	for {
		tc, err = net.Dial("tcp4", net.JoinHostPort("127.0.0.1", itoa(local)))
		if err == nil {
			tc.SetDeadline(time.Now().Add(5 * time.Second))
			if _, err = tc.Write([]byte("hello room")); err == nil {
				got := make([]byte, 10)
				if _, err = io.ReadFull(tc, got); err == nil && string(got) == "hello room" {
					break
				}
			}
			tc.Close()
		}
		if time.Now().After(deadline) {
			t.Fatalf("no TCP echo through the tunnel: %v", err)
		}
		time.Sleep(200 * time.Millisecond)
	}
	tc.Close()

	// UDP: voice-sized and full-size screen pieces, boundaries kept.
	dst := &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1), Port: int(local)}
	a, err := net.DialUDP("udp4", nil, dst)
	if err != nil {
		t.Fatal(err)
	}
	defer a.Close()
	udpEcho(t, a, bytes.Repeat([]byte{0x20}, 170))
	udpEcho(t, a, bytes.Repeat([]byte{0x30}, tailcat.MaxUDPPayload))
	udpEcho(t, a, []byte{0x10})

	b, err := net.DialUDP("udp4", nil, dst)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	udpEcho(t, b, []byte("second member"))
	if n := room.sources(); n != 2 {
		t.Fatalf("room saw %d UDP sources, want 2 (one per member)", n)
	}
}

func TestArgsAreStrict(t *testing.T) {
	if _, err := parseServeArgs([]string{"--key=new", "5000"}); err != nil {
		t.Fatal(err)
	}
	for _, bad := range [][]string{
		{"5000"},
		{"--psk=false", "--key=new", "5000"},
		{"--key=default", "5000"},
		{"--key=new", "5000,5001"},
		{"--key=new", "0"},
	} {
		if _, err := parseServeArgs(bad); err == nil {
			t.Errorf("serve %v: want an error", bad)
		}
	}
	addr := "tc" + string(bytes.Repeat([]byte("a"), 60))
	if _, l, r, err := parseForwardArgs([]string{addr, "4000:5000"}); err != nil || l != 4000 || r != 5000 {
		t.Fatalf("forward: %v %v %v", l, r, err)
	}
	for _, bad := range [][]string{
		{"--bind=0.0.0.0", addr, "4000:5000"},
		{addr, "4000"},
		{addr, "4000:5000", "4001:5001"},
		{"nottc", "4000:5000"},
	} {
		if _, _, _, err := parseForwardArgs(bad); err == nil {
			t.Errorf("forward %v: want an error", bad)
		}
	}
}

func itoa(p uint16) string { return strconv.Itoa(int(p)) }
