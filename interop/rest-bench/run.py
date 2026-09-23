#!/usr/bin/env python3
"""Small, sequential Go/Kimojio TCP comparison. No server-side instrumentation."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]

def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def core(cpu):
    root = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
    return ((root / "physical_package_id").read_text().strip(), (root / "core_id").read_text().strip())

def sample(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    ticks = os.sysconf("SC_CLK_TCK")
    return {"unix_ns": time.time_ns(), "user": int(fields[11])/ticks,
            "system": int(fields[12])/ticks, "rss_bytes": int(fields[21])*os.sysconf("SC_PAGE_SIZE"),
            "threads": int(fields[17])}

def interpolate(samples, at, key):
    for left, right in zip(samples, samples[1:]):
        if left["unix_ns"] <= at <= right["unix_ns"]:
            ratio = (at-left["unix_ns"])/(right["unix_ns"]-left["unix_ns"])
            return left[key] + ratio*(right[key]-left[key])
    raise ValueError("server CPU samples do not bracket client measurement")

def validate(result, concurrency):
    if result.get("valid") is not True or result.get("errors") or result.get("successes",0)<=0:
        raise ValueError(f"invalid load result: {result.get('errors')}")
    if result["attempts"] != result["successes"] or result["new_connections_total"] != concurrency or result["measured_new_connections"] != 0:
        raise ValueError("errors or reconnects invalidate the run")
    if not math.isfinite(result["measured_seconds"]) or result["measured_seconds"] <= 0:
        raise ValueError("invalid measurement duration")
    if not math.isfinite(result["requests_per_second"]):
        raise ValueError("invalid throughput")
    rate = result["successes"]/result["measured_seconds"]
    if abs(rate-result["requests_per_second"]) > rate*1e-9:
        raise ValueError("throughput denominator mismatch")

def trial(name, concurrency, index, args, directory, binaries):
    prefix = directory / f"{index}-{name}-c{concurrency}"
    env = dict(os.environ, GOMAXPROCS="1", GOGC="100", GOMEMLIMIT="off")
    command = ["taskset", "-c", str(args.server_cpu), str(binaries[name])]
    if name == "go": command += ["serve"]
    command += ["--bind", "127.0.0.1:0"]
    row = {"server": name, "concurrency": concurrency, "trial": index, "server_command": command, "valid": False}
    with prefix.with_suffix(".server.stdout").open("w") as out, prefix.with_suffix(".server.stderr").open("w") as err:
        server = subprocess.Popen(command, stdout=out, stderr=err, env=env, cwd=ROOT)
        client = None
        try:
            until = time.monotonic()+10
            address = None
            while time.monotonic()<until:
                if server.poll() is not None: raise RuntimeError("server exited during startup")
                for line in prefix.with_suffix(".server.stdout").read_text().splitlines():
                    if line.startswith("LISTEN "): address=line.split()[1]
                if address: break
                time.sleep(.02)
            if not address: raise RuntimeError("no readiness address")
            load_command = ["taskset", "-c", args.client_cpus, str(binaries["go"]), "load", "--address", address,
                            "--concurrency", str(concurrency), "--warmup", f"{args.warmup}s", "--duration", f"{args.duration}s"]
            row["client_command"] = load_command
            client_env = dict(os.environ, GOMAXPROCS=str(len(args.client_cpus.split(','))), GOGC="100", GOMEMLIMIT="off")
            samples = [sample(server.pid)]
            client = subprocess.Popen(load_command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=client_env, cwd=ROOT)
            limit = time.monotonic()+args.warmup+args.duration+20
            while client.poll() is None:
                if time.monotonic()>limit: raise RuntimeError("load watchdog expired")
                if server.poll() is not None: raise RuntimeError("server exited during load")
                samples.append(sample(server.pid)); time.sleep(.05)
            stdout, stderr = client.communicate(timeout=3)
            prefix.with_suffix(".client.stdout").write_text(stdout)
            prefix.with_suffix(".client.stderr").write_text(stderr)
            samples.append(sample(server.pid))
            time.sleep(.2)
            row["server_after_load"] = sample(server.pid)
            prefix.with_suffix(".samples.json").write_text(json.dumps(samples,indent=2)+"\n")
            if client.returncode: raise RuntimeError(f"load failed ({client.returncode}): {stderr} {stdout[:500]}")
            result = json.loads(stdout); validate(result, concurrency)
            begin, end = result["start_unix_ns"], result["end_unix_ns"]
            cpu = {key:interpolate(samples,end,key)-interpolate(samples,begin,key) for key in ["user","system"]}
            row.update(valid=True, result=result, server_cpu=cpu,
                server_cpu_us_per_request=1e6*sum(cpu.values())/result["successes"],
                server_cpu_utilization=sum(cpu.values())/result["measured_seconds"],
                server_peak_sampled_rss=max(s["rss_bytes"] for s in samples),
                server_peak_threads=max(s["threads"] for s in samples),
                client_cpu_utilization=(result["client_user_seconds"]+result["client_system_seconds"])/result["measured_seconds"]/len(args.client_cpus.split(',')))
            if row["client_cpu_utilization"] > .85:
                row["warning"] = "load generator CPU above 85% of its allocated cores"
            for key,path in binaries.items():
                if digest(path) != args.hashes[key]: raise RuntimeError("binary changed during measurement")
            # Successful requests must not mask transport/driver failures.
            if prefix.with_suffix(".server.stderr").read_text().strip():
                raise RuntimeError("server wrote diagnostics; inspect stderr before accepting run")
        except Exception as exc:
            row.update(valid=False, error=str(exc))
        finally:
            if client is not None and client.poll() is None:
                client.kill(); client.communicate()
            if server.poll() is None: server.terminate()
            try: server.wait(timeout=3)
            except subprocess.TimeoutExpired: server.kill(); server.wait()
    return row

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument("--go",type=Path,default=ROOT/"target/rest-bench/go-rest")
    p.add_argument("--kimojio",type=Path,default=ROOT/"target/rest-bench/rust/release/examples/rest_bench")
    p.add_argument("--output",type=Path,required=True)
    p.add_argument("--server-cpu",type=int,default=13)
    p.add_argument("--client-cpus",default="16,18,20,22")
    p.add_argument("--concurrency",default="1,16")
    p.add_argument("--trials",type=int,default=3)
    p.add_argument("--duration",type=int,default=10)
    p.add_argument("--warmup",type=int,default=2)
    args=p.parse_args()
    cpus=list(map(int,args.client_cpus.split(','))); levels=list(map(int,args.concurrency.split(',')))
    if not 1<=args.trials<=10 or not 1<=args.duration<=30 or not 1<=args.warmup<=10 or not all(1<=c<=32 for c in levels): p.error("invalid bounds")
    if len(set(cpus))!=len(cpus) or len({core(c) for c in cpus})!=len(cpus) or core(args.server_cpu) in {core(c) for c in cpus}: p.error("CPU placement overlaps physical cores")
    args.output.mkdir(parents=True,exist_ok=False)
    binaries={"go":args.go.resolve(strict=True), "kimojio":args.kimojio.resolve(strict=True)}
    args.hashes={k:digest(v) for k,v in binaries.items()}
    report={"schema":1,"complete":False,"valid":False,"binary_sha256":args.hashes,
        "fixture_sha256":{f.name:digest(f) for f in (ROOT/"interop/rest-bench/fixtures").glob('*')},
        "server_cpu":args.server_cpu,"client_cpus":cpus,"concurrency":levels,
        "duration_seconds":args.duration,"warmup_seconds":args.warmup,"trials":args.trials,
        "kernel":platform.release(),"cpu_topology":{str(c):core(c) for c in [args.server_cpu,*cpus]},
        "initial_load_average":os.getloadavg(),
        "go_version":subprocess.check_output(["go","version"],text=True).strip(),
        "rustc_version":subprocess.check_output(["rustc","-V"],text=True).strip(),
        "rows":[]}
    path=args.output/"report.json"
    for index in range(args.trials):
        for concurrency in levels:
            for name in (["go","kimojio"] if index%2==0 else ["kimojio","go"]):
                row=trial(name,concurrency,index,args,args.output,binaries);report["rows"].append(row)
                path.write_text(json.dumps(report,indent=2)+"\n")
                print(name,concurrency,index,row.get('result',{}).get('requests_per_second'),row.get('error',''),flush=True)
    report["complete"]=True;report["valid"]=all(r["valid"] for r in report["rows"])
    summaries=[]
    if report["valid"]:
        for c in levels:
            for name in binaries:
                rows=[r for r in report["rows"] if r["server"]==name and r["concurrency"]==c]
                summary={"server":name,"concurrency":c}
                for key,values in {
                    "rps":[r["result"]["requests_per_second"] for r in rows],
                    "p50_us":[r["result"]["p50_us"] for r in rows],
                    "p99_us":[r["result"]["p99_us"] for r in rows],
                    "server_cpu_us_per_request":[r["server_cpu_us_per_request"] for r in rows],
                    "server_cpu_utilization":[r["server_cpu_utilization"] for r in rows],
                    "server_rss_mib":[r["server_peak_sampled_rss"]/2**20 for r in rows],
                    "client_cpu_utilization":[r["client_cpu_utilization"] for r in rows],
                }.items(): summary[key]={"median":statistics.median(values),"min":min(values),"max":max(values)}
                summaries.append(summary)
    report["summaries"]=summaries;path.write_text(json.dumps(report,indent=2)+"\n")
    return 0 if report["valid"] else 1
if __name__=="__main__": raise SystemExit(main())
