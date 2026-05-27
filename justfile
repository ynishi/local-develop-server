# Test justfile for allow-agent boundary verification

[group('allow-agent')]
hello:
    echo "hello from allow-agent recipe"

[group('allow-agent')]
check:
    cargo check

# This recipe should NOT be exposed in agent-only mode
dangerous:
    echo "this should be blocked"

# Legacy format test
# [allow-agent]
legacy-hello:
    echo "hello from legacy allow-agent"
