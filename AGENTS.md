# Startup Context

At the beginning of each new chat for this repository, load the local Claude and OpenCode context before doing substantive work.

1. Determine the absolute path of the repository root.
2. Compute the Claude project key from that absolute path by replacing every `/` with `-`.
   Example: `/home/user/projects/multi-agent-orchestrator-rs` -> `-home-user-projects-multi-agent-orchestrator-rs`.
3. Read the project memory file at `$HOME/.claude/projects/<project_key>/memory/MEMORY.md`.
4. Read `CLAUDE.md` in the repository root.

Implementation notes:

- If the memory file is missing, note that briefly and continue with the available context.
- Treat this startup read as required context gathering, not as an optional hint.
