"""The only frozen sidecar entry point. No eval, plugin or provider gateway."""
from agent_core.worker import main

if __name__ == '__main__':
    raise SystemExit(main())
