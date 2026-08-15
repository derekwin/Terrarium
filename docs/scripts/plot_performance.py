#!/usr/bin/env python3
"""Render the performance figures for docs/performance.md and README.

Inputs (raw measurements, committed):
  docs/workload-overhead-2026-08-15-vmm.json — 5 envs x 4 workloads, medians
  docs/bench-privilege-drop-2026-08-15.json — vmm vs legacy A/B (normal order)
  docs/bench-privilege-drop-swap-2026-08-15.json — A/B with swapped order

Outputs:
  docs/perf/exec-path-latency.png       — per-call fixed cost, Terrarium vs
                                          docker exec vs gVisor runsc do
  docs/perf/workload-overhead.png       — workload runtimes normalized to bare
  docs/perf/governance-overhead.png     — vm+confine / vm ratio per workload
  docs/perf/privdrop-ab.png             — privilege-drop A/B (exec p50,
                                          cold create, restore)

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
    ax.annotate(
        "~31x faster than docker exec,\n~101x faster than gVisor do",
        xy=(0, 293.3), xytext=(0.45, 150),
        fontsize=9, color="#333",
        arrowprops=dict(arrowstyle="->", color="#333"),
    )
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


def main() -> None:
    fig_exec_path()
    fig_workload_overhead()
    fig_governance()
    fig_privdrop_ab()
    print("wrote:", [p.name for p in sorted(OUT.glob("*.png"))])


if __name__ == "__main__":
    main()
