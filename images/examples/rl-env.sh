#!/bin/sh
# RL environment layer setup (example).
#
# Convention (docs/strategy.md P1 / docs/benchmarks.md in-place reset):
# the environment's READY state lives in a LAYER; episode writes go to
# the VM's writable upper (/workdir, /tmp, /run) and are cleared by the
# in-place episode reset (guest-proxy `reset`). Building the ready state
# into a layer is what makes the fast reset correct.
#
# Usage:
#   terra tool create -n rl-env --template alpine --no-net \
#       --script images/examples/rl-env.sh
#   terra sandbox create --tenant my-env --layers rl-env   (SDK: layers=["rl-env"])

set -e

echo "baking RL environment ready-state into the layer"

# Ready-state marker: baked into the LAYER, so it survives every in-place
# episode reset (the reset clears the upper, never the layer).
echo "ready" > /var/rl-ready

# Task machinery lives in the layer too — the training loop runs it each
# episode. Contract: read /workdir/input.json (optional), write the result
# to /workdir/output.json, echo it to stdout (what the loop collects).
mkdir -p /usr/local/bin
cat > /usr/local/bin/rl-task <<'EOF'
#!/bin/sh
# Example RL task: reads the episode input (if any), computes, writes the
# result to /workdir/output.json and echoes it for collection.
i=100
if [ -f /workdir/input.json ]; then
  n=$(sed -n 's/.*"x"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' /workdir/input.json | head -1)
  [ -n "$n" ] && i=$n
fi
j=0
while [ $j -lt 100 ]; do j=$((j+1)); done
echo "result-$i" | tee /workdir/output.json
EOF
chmod +x /usr/local/bin/rl-task

echo "rl-env layer ready-state baked"
