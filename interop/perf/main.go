package main

import (
	"encoding/json"
	"fmt"
	"os"
	"runtime"
	"syscall"
	"time"
)

func main() {
	if len(os.Args) < 2 {
		fatal(fmt.Errorf("expected http-load, ws-load, http-serve, or ws-serve"))
	}
	switch os.Args[1] {
	case "http-serve":
		fatal(serveHTTP(os.Args[2:]))
	case "ws-serve":
		fatal(serveWS(os.Args[2:]))
	case "http-load", "ws-load":
		started := time.Now()
		var result report
		var err error
		if os.Args[1] == "http-load" {
			result, err = loadHTTP(os.Args[2:])
		} else {
			result, err = loadWS(os.Args[2:])
		}
		if err != nil {
			result.Errors = append(result.Errors, err.Error())
		}
		result.Schema = 1
		result.Valid = len(result.Errors) == 0 && result.Successes > 0 && result.ElapsedSeconds > 0
		result.InvocationSeconds = time.Since(started).Seconds()
		var usage syscall.Rusage
		if err := syscall.Getrusage(syscall.RUSAGE_SELF, &usage); err != nil {
			result.Errors = append(result.Errors, err.Error())
			result.Valid = false
		}
		result.ProcessCPUUserSeconds = float64(usage.Utime.Nano()) / 1e9
		result.ProcessCPUSystemSeconds = float64(usage.Stime.Nano()) / 1e9
		result.MaxRSSKiB = usage.Maxrss
		result.VoluntarySwitches = usage.Nvcsw
		result.InvoluntarySwitches = usage.Nivcsw
		result.GOMAXPROCS = runtime.GOMAXPROCS(0)
		result.GoVersion = runtime.Version()
		descriptors, descriptorErr := os.ReadDir("/proc/self/fd")
		if descriptorErr != nil {
			result.Errors = append(result.Errors, descriptorErr.Error())
			result.Valid = false
		}
		result.FinalFDs = len(descriptors)
		var memory runtime.MemStats
		runtime.ReadMemStats(&memory)
		result.FinalHeapAllocBytes = memory.HeapAlloc
		result.FinalHeapInuseBytes = memory.HeapInuse
		result.TotalAllocatedBytes = memory.TotalAlloc
		result.P50MS = result.Histogram.quantile(.50)
		result.P95MS = result.Histogram.quantile(.95)
		result.P99MS = result.Histogram.quantile(.99)
		if result.Valid {
			rate := float64(result.Successes) / result.ElapsedSeconds
			result.SuccessesPerSecond = &rate
		}
		if err := json.NewEncoder(os.Stdout).Encode(result); err != nil {
			fatal(err)
		}
		if !result.Valid {
			os.Exit(1)
		}
	default:
		fatal(fmt.Errorf("unknown mode %q", os.Args[1]))
	}
}

func fatal(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
