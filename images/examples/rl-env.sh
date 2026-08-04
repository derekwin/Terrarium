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

# Task machinery lives in the layer too — the agent runs it each episode.
mkdir -p /usr/local/bin
cat > /usr/local/bin/rl-task <<'EOF'
#!/bin/sh
# Example RL task: computes over the episode, writes the result.
i=0
while [ $i -lt 100 ]; do i=$((i+1)); done
echo "result-$i"
EOF
chmod +x /usr/local/bin/rl-task

echo "rl-env layer ready-state baked"
