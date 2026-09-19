package main

import (
	"fmt"
	"syscall"
	"time"
)

type measurement struct {
	started time.Time
	usage   syscall.Rusage
}

func beginMeasurement() (measurement, error) {
	var baseline syscall.Rusage
	if err := syscall.Getrusage(syscall.RUSAGE_SELF, &baseline); err != nil {
		return measurement{}, err
	}
	return measurement{started: time.Now(), usage: baseline}, nil
}

func finishMeasurement(result *report, window measurement) {
	var usage syscall.Rusage
	err := syscall.Getrusage(syscall.RUSAGE_SELF, &usage)
	ended := time.Now()
	recordMeasurement(result, window, usage, ended, err)
}

func recordMeasurement(result *report, window measurement, usage syscall.Rusage, ended time.Time, err error) {
	result.ElapsedSeconds = ended.Sub(window.started).Seconds()
	result.MeasurementStartUnixNS = window.started.UnixNano()
	result.MeasurementEndUnixNS = ended.UnixNano()
	result.MeasurementScope = "actual_admission_boundary_through_worker_drain_and_explicit_connection_cleanup"
	if err != nil {
		result.Errors = append(result.Errors, fmt.Sprintf("measurement CPU: %v", err))
		return
	}
	result.CPUUserSeconds = float64(usage.Utime.Nano()-window.usage.Utime.Nano()) / 1e9
	result.CPUSystemSeconds = float64(usage.Stime.Nano()-window.usage.Stime.Nano()) / 1e9
	if result.CPUUserSeconds < 0 || result.CPUSystemSeconds < 0 || result.ElapsedSeconds <= 0 {
		result.Errors = append(result.Errors, "invalid measurement CPU/time window")
	}
}
