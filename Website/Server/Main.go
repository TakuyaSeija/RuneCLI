// atoryn-studio-web — the RuneAgent website at <origin>/Studio/.
//
// One binary per release, the pages and assets embedded. It binds loopback
// only (DEV may add a private LAN address); the public reaches PROD through the
// parent nginx and the tunnel. It holds no secret and opens no outbound
// connection: the only things it reads at run time are its config and the
// signed releases folder.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	cfgPath := flag.String("config", "", "config file")
	instance := flag.Int("instance", 0, "PROD blue/green port N: site on 127.0.0.1:N, health on 127.0.0.1:N+2")
	check := flag.Bool("check", false, "validate config, pages and catalog, then exit")
	flag.Parse()
	log.SetFlags(0) // the journal stamps the time

	cfg, err := loadConfig(*cfgPath)
	if err == nil {
		if *instance != 0 {
			if *instance < 1024 || *instance > 65533 {
				err = fmt.Errorf("instance port out of range")
			}
			cfg.Listen = []string{fmt.Sprintf("127.0.0.1:%d", *instance)}
			cfg.Health = fmt.Sprintf("127.0.0.1:%d", *instance+2)
		}
		if err == nil {
			err = cfg.validate()
		}
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "config:", err)
		os.Exit(1)
	}
	st, err := newSite(cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, "site:", err)
		os.Exit(1)
	}
	dl, err := os.OpenRoot(cfg.Downloads)
	if err != nil {
		fmt.Fprintln(os.Stderr, "downloads:", err)
		os.Exit(1)
	}
	cat, err := openCatalog(cfg.Downloads)
	if err != nil {
		fmt.Fprintln(os.Stderr, "catalog:", err)
		os.Exit(1)
	}
	if *check {
		fmt.Printf("ok: %s site %s, %d release(s)\n", cfg.Env, st.build, len(cat.get().Releases))
		return
	}

	stop := make(chan struct{})
	go cat.watch(30*time.Second, stop)
	srv := newServer(cfg, st, cat, dl)
	handler := srv.routes()

	var servers []*http.Server
	errc := make(chan error, len(cfg.Listen)+1)
	serve := func(addr string, h http.Handler) {
		ln, err := net.Listen("tcp", addr)
		if err != nil {
			fmt.Fprintln(os.Stderr, "listen:", err)
			os.Exit(1)
		}
		hs := &http.Server{
			Handler:           h,
			ReadHeaderTimeout: 5 * time.Second,
			ReadTimeout:       10 * time.Second,
			WriteTimeout:      120 * time.Second,
			IdleTimeout:       60 * time.Second,
			MaxHeaderBytes:    16 << 10,
			ErrorLog:          log.New(os.Stderr, "http: ", 0),
		}
		servers = append(servers, hs)
		go func() { errc <- hs.Serve(ln) }()
	}
	for _, a := range cfg.Listen {
		serve(a, handler)
	}
	healthMux := http.NewServeMux()
	healthMux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		w.Header().Set("Cache-Control", "no-store")
		fmt.Fprintf(w, "ok %s %d\n", st.build, len(cat.get().Releases))
	})
	serve(cfg.Health, healthMux)
	log.Printf("serving %s site %s on %v (health %s)", cfg.Env, st.build, cfg.Listen, cfg.Health)

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGTERM, syscall.SIGINT)
	select {
	case s := <-sig:
		log.Printf("%v: draining", s)
	case err := <-errc:
		if !errors.Is(err, http.ErrServerClosed) {
			log.Printf("serve: %v", err)
		}
	}
	close(stop)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	for _, hs := range servers {
		hs.Shutdown(ctx)
	}
}
