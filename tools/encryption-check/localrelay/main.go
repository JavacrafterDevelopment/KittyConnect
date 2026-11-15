// A local TailCat relay (DERP) and STUN server for the encryption test, in
// plain HTTP rather than TLS, so a packet capture sees exactly what a relay
// would: if anything weren't encrypted by WireGuard, it would show here.
// Prints the DERP map URL (for TAILCAT_DERPMAP_URL) and the region JSON
// (for the test VOIP helper), then serves until killed.
package main

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"

	"tailscale.com/derp/derpserver"
	"tailscale.com/net/stun"
	"tailscale.com/tailcfg"
	"tailscale.com/types/key"
	"tailscale.com/types/logger"
)

func main() {
	d := derpserver.New(key.NewNode(), logger.Discard)
	ln, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	uln, err := net.ListenUDP("udp4", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		panic(err)
	}
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
	region := &tailcfg.DERPRegion{
		RegionID:   900,
		RegionCode: "test",
		RegionName: "Local test relay",
		Nodes: []*tailcfg.DERPNode{{
			Name:             "900a",
			RegionID:         900,
			HostName:         "127.0.0.1",
			IPv4:             "127.0.0.1",
			IPv6:             "none",
			STUNPort:         uln.LocalAddr().(*net.UDPAddr).Port,
			DERPPort:         ln.Addr().(*net.TCPAddr).Port,
			InsecureForTests: true,
		}},
	}
	dm := &tailcfg.DERPMap{Regions: map[tailcfg.DERPRegionID]*tailcfg.DERPRegion{900: region}}
	mux := http.NewServeMux()
	mux.Handle("/derp", derpserver.Handler(d))
	mux.HandleFunc("/derpmap.json", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(dm)
	})
	rj, _ := json.Marshal(region)
	fmt.Printf("DERPMAP=http://%s/derpmap.json\n", ln.Addr())
	fmt.Printf("REGION=%s\n", rj)
	fmt.Printf("DERPPORT=%d\n", ln.Addr().(*net.TCPAddr).Port)
	os.Stdout.Sync()
	panic(http.Serve(ln, mux))
}
