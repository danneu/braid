package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"os"
)

func main() {
	socketPath := os.Getenv("BTRNAS_SOCKET")
	if socketPath == "" {
		socketPath = "/run/btrnas/daemon.sock"
	}

	// Remove stale socket file
	os.Remove(socketPath)

	ln, err := net.Listen("unix", socketPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "btrnasd: listen: %v\n", err)
		os.Exit(1)
	}
	defer ln.Close()

	fmt.Fprintf(os.Stderr, "btrnasd: listening on %s\n", socketPath)

	for {
		conn, err := ln.Accept()
		if err != nil {
			fmt.Fprintf(os.Stderr, "btrnasd: accept: %v\n", err)
			continue
		}
		go handleConn(conn)
	}
}

func handleConn(conn net.Conn) {
	defer conn.Close()

	scanner := bufio.NewScanner(conn)
	for scanner.Scan() {
		var req struct {
			Method string `json:"method"`
		}
		if err := json.Unmarshal(scanner.Bytes(), &req); err != nil {
			fmt.Fprintf(conn, "{\"error\":\"invalid json\"}\n")
			continue
		}

		switch req.Method {
		case "ping":
			fmt.Fprintf(conn, "{\"status\":\"ok\"}\n")
		default:
			fmt.Fprintf(conn, "{\"error\":\"unknown method: %s\"}\n", req.Method)
		}
	}
}
