// kc-voip is KittyConnect's tunnel for VOIP rooms. It is built on the TailCat
// library (github.com/tailscale/tailcat), the same code as the official
// tailcat.exe that KittyConnect still uses for Ports and File Share, and takes
// the same two commands KittyConnect gives that one:
//
//	kc-voip [--verbose] serve --key=new <port>
//	kc-voip [--verbose] forward <tc-addr> <local-port>:<remote-port>
//
// The difference is UDP. The official command line forwards TCP only; this
// carries TCP *and* UDP on the port, both inside the same WireGuard tunnel.
// Rooms use TCP for names, chat and attachments, and UDP for voice and screen
// video, so a lost packet never holds up what's behind it.
//
// It prints the same lines the official CLI does (the address, and with
// --verbose TailCat's own "now using" and "got meow" logs), so KittyConnect
// reads both the same way. When the official tailcat can serve UDP itself,
// KittyConnect can go back to it and this helper can be deleted.
//
// Rules this file keeps, and tunnel_test.go checks:
//   - Everything local is on 127.0.0.1. There is no --bind and no way to
//     listen anywhere else.
//   - The WireGuard pre-shared key is always on. There is no --psk option.
//   - Every room gets a new ephemeral key (--key=new is the only key mode).
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/signal"
	"runtime/debug"
	"strconv"
	"strings"
	"syscall"

	"github.com/tailscale/tailcat"
	"tailscale.com/types/logger"
)

func usage() {
	fmt.Fprintln(os.Stderr, "usage:")
	fmt.Fprintln(os.Stderr, "  kc-voip [--verbose] serve --key=new <port>")
	fmt.Fprintln(os.Stderr, "  kc-voip [--verbose] forward <tc-addr> <local-port>:<remote-port>")
	fmt.Fprintln(os.Stderr, "  kc-voip version")
	os.Exit(2)
}

func main() {
	args := os.Args[1:]
	verbose := false
	if len(args) > 0 && args[0] == "--verbose" {
		verbose = true
		args = args[1:]
	}
	if len(args) == 0 {
		usage()
	}
	logf := logger.Discard
	if verbose {
		// Same as the official CLI: TailCat's networking logs, including
		// the "now using <ip:port>" line that marks a direct path.
		tailcat.Verbose = true
		logf = log.Printf
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	switch args[0] {
	case "version":
		fmt.Println(versionString())
		return
	case "serve":
		port, err := parseServeArgs(args[1:])
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			usage()
		}
		s, _, err := serve(ctx, logf, port, nil)
		if err != nil {
			fatal(err)
		}
		defer s.Close()
	case "forward":
		addr, local, remote, err := parseForwardArgs(args[1:])
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			usage()
		}
		f, err := forward(ctx, logf, addr, local, remote)
		if err != nil {
			fatal(err)
		}
		defer f.Close()
	default:
		usage()
	}
	<-ctx.Done()
}

// fatal reports err and exits. It's printed without a timestamp, as the
// official CLI's own messages are, so KittyConnect shows it as the reason
// rather than taking it for networking chatter.
func fatal(err error) {
	fmt.Fprintf(os.Stderr, "kc-voip: %v\n", err)
	os.Exit(1)
}

// parseServeArgs accepts exactly "--key=new <port>".
func parseServeArgs(args []string) (uint16, error) {
	if len(args) != 2 || args[0] != "--key=new" {
		return 0, fmt.Errorf("serve takes exactly --key=new and one port")
	}
	return parsePort(args[1])
}

// parseForwardArgs accepts exactly "<tc-addr> <local>:<remote>".
func parseForwardArgs(args []string) (tailcat.Addr, uint16, uint16, error) {
	if len(args) != 2 {
		return "", 0, 0, fmt.Errorf("forward takes an address and one local:remote mapping")
	}
	if !strings.HasPrefix(args[0], "tc") {
		return "", 0, 0, fmt.Errorf("%q is not a tailcat address", args[0])
	}
	l, r, ok := strings.Cut(args[1], ":")
	if !ok {
		return "", 0, 0, fmt.Errorf("mapping %q must be local:remote", args[1])
	}
	local, err := parsePort(l)
	if err != nil {
		return "", 0, 0, err
	}
	remote, err := parsePort(r)
	if err != nil {
		return "", 0, 0, err
	}
	return tailcat.Addr(args[0]), local, remote, nil
}

func parsePort(s string) (uint16, error) {
	p, err := strconv.ParseUint(s, 10, 16)
	if err != nil || p == 0 {
		return 0, fmt.Errorf("invalid port %q", s)
	}
	return uint16(p), nil
}

// versionString names this helper and the TailCat library it was built from.
func versionString() string {
	tc := "unknown"
	if bi, ok := debug.ReadBuildInfo(); ok {
		for _, d := range bi.Deps {
			if d.Path == "github.com/tailscale/tailcat" {
				tc = d.Version
			}
		}
	}
	return "kc-voip (TailCat library " + tc + ")"
}
