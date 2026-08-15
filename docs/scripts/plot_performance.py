#!/usr/bin/env python3
"""Render the performance figures for docs/performance.md and README.

Inputs (raw measurements, committed):
  docs/workload-overhead-2026-08-15-vmm.json — 5 envs x 4 workloads, medians
  docs/bench-privilege-drop-2026-08-15.json — vmm vs legacy A/B (normal order)
  docs/bench-privilege-drop-swap-2026-08-15.json — A/B with swapped order
  docs/density-compare-2026-08-15.json    — 100-instance sweep, terra vs
                                            docker vs gVisor (creation rate,
                                            host memory, exec throughput)
  docs/density-compare-snapshot-2026-08-15.json — same sweep, terra via
                                            snapshot restore (the RL/density
                                            fast-create path)
  docs/benchmark-results-2026-08-03-{base,ubuntu}-12.json — per-VM RSS/Pss
                                            memory-sharing curves

Outputs:
  docs/perf/exec-path-latency.png       — per-call fixed cost, Terrarium vs
                                          docker exec vs gVisor runsc do
  docs/perf/workload-overhead.png       — workload runtimes normalized to bare
  docs/perf/governance-overhead.png     — vm+confine / vm ratio per workload
  docs/perf/privdrop-ab.png             — privilege-drop A/B (exec p50,
                                          cold create, restore)
  docs/perf/density-compare.png         — terra vs docker vs gVisor:
                                          create rate / memory / execs
  docs/perf/memory-sharing.png          — per-VM RSS vs Pss as tenants grow
                                          (shared layer page cache)

Uses English labels (no CJK fonts guaranteed in CI).
"""

from __future__ import annotations

import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
DOCS = REPO / "docs"
OUT = DOCS / "perf"
OUT.mkdir(parents=True, exist_ok=True)


def _load(name: str) -> dict:
    return json.loads((DOCS / name).read_text())


def fig_exec_path() -> None:
    # Measured 2026-08-15: Terrarium warm VM exec p50 (bench A/B), docker
    # exec echo p50 (long-running container), gVisor runsc do echo p50
    # (one-shot sandbox, includes startup).
    labels = ["Terrarium\nexec (warm VM)", "docker\nexec", "gVisor\nrunsc do"]
    ms = [2.9, 88.9, 293.3]
    colors = ["#2ca02c", "#ff7f0e", "#d62728"]
    fig, ax = plt.subplots(figsize=(6.4, 3.6))
    bars = ax.bar(labels, ms, color=colors, width=0.6)
    ax.set_ylabel("per-call latency (ms, p50, log scale)")
    ax.set_yscale("log")
    ax.set_title("One agent tool call: fixed cost of the exec path")
    for b, v in zip(bars, ms):
        ax.text(b.get_x() + b.get_width() / 2, v * 1.15, f"{v:.1f} ms",
                ha="center", va="bottom", fontsize=10)
    # Multiples vs Terrarium, labeled directly on the comparison bars.
    for b, v, mult in zip(bars, ms, [1.0, 88.9 / 2.9, 293.3 / 2.9]):
        if mult > 1.0:
            ax.text(b.get_x() + b.get_width() / 2, v * 1.45,
                    f"\u2248{mult:.0f}\u00d7 slower than Terrarium",
                    ha="center", va="bottom", fontsize=8, color="#555")
    ax.set_ylim(1, 1200)
    ax.grid(axis="y", linestyle=":", alpha=0.5)
    fig.tight_layout()
    fig.savefig(OUT / "exec-path-latency.png", dpi=150)
    plt.close(fig)


def fig_workload_overhead() -> None:
    data = _load("workload-overhead-2026-08-15-vmm.json")
    workloads = list(data)
    envs = ["bare", "vm", "vm+confine", "docker", "gvisor"]
    env_colors = {"bare": "#7f7f7f", "vm": "#1f77b4", "vm+confine": "#2ca02c",
                  "docker": "#ff7f0e", "gvisor": "#d62728"}
    # normalize to bare
    ratios = {
        w: {
            e: round(data[w][e]["median_ms"] / data[w]["bare"]["median_ms"], 2)
            for e in envs
        }
        for w in workloads
    }
    x = range(len(workloads))
    width = 0.15
    fig, ax = plt.subplots(figsize=(9, 4.2))
    for i, e in enumerate(envs):
        vals = [ratios[w][e] for w in workloads]
        bars = ax.bar([p + i * width for p in x], vals, width,
                      label=e, color=env_colors[e])
        for b, v in zip(bars, vals):
            ax.text(b.get_x() + b.get_width() / 2, v + 0.35, f"{v:.1f}x",
                    ha="center", va="bottom", fontsize=7.5, rotation=90)
    ax.set_xticks([p + 2 * width for p in x])
    ax.set_xticklabels(workloads)
    ax.set_ylabel("runtime vs bare host (x, lower is better)")
    ax.set_title("Real agent-style workloads: Terrarium vs docker vs gVisor\n"
                 "(static C probe, same work in every environment, median of 5)")
    ax.axhline(1.0, color="#666", lw=0.8, ls="--")
    ax.legend(ncol=5, fontsize=8, loc="upper left")
    ax.grid(axis="y", linestyle=":", alpha=0.4)
    fig.tight_layout()
    fig.savefig(OUT / "workload-overhead.png", dpi=150)
    plt.close(fig)


def fig_governance() -> None:
    data = _load("workload-overhead-2026-08-15-vmm.json")
    workloads = list(data)
    ratios = [
        round(data[w]["vm+confine"]["median_ms"] / data[w]["vm"]["median_ms"], 2)
        for w in workloads
    ]
    fig, ax = plt.subplots(figsize=(6.4, 3.4))
    bars = ax.bar(workloads, ratios, color="#2ca02c", width=0.55)
    ax.axhline(1.0, color="#666", lw=1, ls="--")
    ax.set_ylabel("vm+confine / vm (x)")
    ax.set_title("Governance (L2 confine) overhead per workload\n"
                 "— all within measurement noise, ≈ 0 cost")
    for b, v in zip(bars, ratios):
        ax.text(b.get_x() + b.get_width() / 2, v + 0.01, f"{v:.2f}x",
                ha="center", fontsize=10)
    ax.set_ylim(0, 1.25)
    ax.grid(axis="y", linestyle=":", alpha=0.4)
    fig.tight_layout()
    fig.savefig(OUT / "governance-overhead.png", dpi=150)
    plt.close(fig)


def fig_privdrop_ab() -> None:
    data = _load("bench-privilege-drop-2026-08-15.json")["results"]
    modes = list(data)
    metrics = [
        ("exec_latency", "exec latency p50 (ms)", 2.8),
        ("cold_create", "cold create p50 (ms)", 22),
        ("restore", "restore p50 (ms)", 22),
    ]
    fig, axes = plt.subplots(1, 3, figsize=(10.5, 3.3))
    for ax, (metric, ylabel, ymax) in zip(axes, metrics):
        vals = [data[m][metric]["p50_ms"] for m in modes]
        bars = ax.bar(modes, vals, color=["#2ca02c", "#7f7f7f"], width=0.55)
        ax.set_title(metric.replace("_", " "))
        ax.set_ylabel(ylabel)
        ax.set_ylim(0, max(ymax, max(vals) * 1.15))
        for b, v in zip(bars, vals):
            ax.text(b.get_x() + b.get_width() / 2, v + 0.5, f"{v:.1f}",
                    ha="center", fontsize=10)
        ax.grid(axis="y", linestyle=":", alpha=0.4)
    fig.suptitle("Privilege-drop A/B (vmm user vs legacy root, same host)\n"
                 "exec path identical; create/restore pay a one-time ~15ms drop-in cost",
                 fontsize=11)
    fig.tight_layout(rect=[0, 0, 1, 0.9])
    fig.savefig(OUT / "privdrop-ab.png", dpi=150)
    plt.close(fig)


def fig_density_compare() -> None:
    data = _load("density-compare-snapshot-2026-08-15.json")["baselines"]
    cold = _load("density-compare-2026-08-15.json")["baselines"]["terra"]
    cold_rate = cold["instances_per_sec"]
    order = ["terra", "docker", "gvisor"]
    labels = {"terra": "Terrarium", "docker": "docker", "gvisor": "gVisor"}
    colors = {"terra": "#2ca02c", "docker": "#ff7f0e", "gvisor": "#d62728"}
    metrics = [
        ("instances_per_sec", "create rate (instances/s)", None),
        ("per_instance_mb", "host memory per instance (MB)", None),
        ("execs_per_sec", "aggregate exec throughput (execs/s)", None),
    ]
    fig, axes = plt.subplots(1, 3, figsize=(11.5, 3.4))
    for ax, (key, ylabel, _) in zip(axes, metrics):
        vals = [data[b][key] for b in order]
        plot_vals = [0.0 if v is None else v for v in vals]
        bars = ax.bar([labels[b] for b in order], plot_vals,
                      color=[colors[b] for b in order], width=0.55)
        for b, v in zip(bars, vals):
            label = "n/a" if v is None else f"{v:.1f}"
            ax.text(b.get_x() + b.get_width() / 2, (v or 0) + max(plot_vals or [1]) * 0.02,
                    label, ha="center", fontsize=10)
        ax.set_ylabel(ylabel)
        ax.set_title(key.replace("_", " "))
        ax.grid(axis="y", linestyle=":", alpha=0.4)
    fig.suptitle(
        "Density, 100 long-lived instances on one host (2026-08-15, vmm drop)\n"
        f"Terrarium via snapshot restore ({cold_rate:.1f}/s cold boot); "
        "Terrarium: real KVM per tenant. docker: shared kernel (0.6 MB/instance "
        "is the container shell, no isolation). gVisor: one-shot sandboxes.",
        fontsize=10,
    )
    fig.tight_layout(rect=[0, 0, 1, 0.88])
    fig.savefig(OUT / "density-compare.png", dpi=150)
    plt.close(fig)


def fig_memory_sharing() -> None:
    fig, axes = plt.subplots(1, 2, figsize=(11.5, 3.7))
    for ax, (label, fname) in zip(
        axes,
        [("base (20 MB layer)", "benchmark-results-2026-08-03-base-12.json"),
         ("ubuntu (99 MB layer)", "benchmark-results-2026-08-03-ubuntu-12.json")],
    ):
        rows = _load(fname)["per_vm_memory_mb"]
        n = [r["tenants"] for r in rows]
        per_rss = [r["per_vm_rss_mb"] for r in rows]
        per_pss = [r["per_vm_pss_mb"] for r in rows]
        shared_pct = [r["shared_pct"] for r in rows]
        ax.plot(n, per_rss, "o-", color="#1f77b4", label="per-VM RSS")
        ax.plot(n, per_pss, "o-", color="#2ca02c", label="per-VM Pss (shared once)")
        ax.set_xlabel("tenants (VMs)")
        ax.set_ylabel("host memory per VM (MB)")
        ax.set_title(label)
        ax.grid(linestyle=":", alpha=0.4)
        ax.legend(fontsize=8)
        ax2 = ax.twinx()
        ax2.plot(n, shared_pct, "s--", color="#d62728", alpha=0.8, label="shared %")
        ax2.set_ylabel("shared page-cache %", color="#d62728")
        ax2.tick_params(axis="y", labelcolor="#d62728")
    fig.suptitle("Layer page-cache sharing: per-VM cost stays flat as tenants grow\n"
                 "(RSS counts shared pages per process; Pss counts them once)",
                 fontsize=11)
    fig.tight_layout(rect=[0, 0, 1, 0.88])
    fig.savefig(OUT / "memory-sharing.png", dpi=150)
    plt.close(fig)


def main() -> None:
    fig_exec_path()
    fig_workload_overhead()
    fig_governance()
    fig_privdrop_ab()
    fig_density_compare()
    fig_memory_sharing()
    print("wrote:", [p.name for p in sorted(OUT.glob("*.png"))])


if __name__ == "__main__":
    main()
