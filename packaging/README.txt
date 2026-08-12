TermBridge
==========

Remote Terminal Runtime for AI Agents.

Quick Start
-----------

1. Place termbridge-mcp and termbridge-auth-helper in the same directory.
2. Import mcp-config.json into your MCP client (TraeCode / Claude Code / Codex / OpenCode).
3. Adjust the "command" path in mcp-config.json to point to termbridge-mcp.
4. Install SKILL.md into your AI Agent's skill directory.
5. Restart your MCP client.

Files
-----

termbridge-mcp            MCP server binary (main entry point)
termbridge                CLI binary (human admin tool, optional)
termbridge-auth-helper    Credential helper (must be in same directory as termbridge-mcp)
mcp-config.json           MCP server configuration template
SKILL.md                  Agent Terminal Protocol operational guide
resources/agentd/         Remote daemon binary (auto-deployed to target host by
                          bootstrap_host, do not run locally; Linux x86_64 only)

First Connection
----------------

Use the bootstrap_host MCP tool for first-time SSH key deployment.
Password is prompted via native OS dialog (never enters LLM context).

Documentation
-------------

Full docs: https://github.com/summerxzp/TermBridge#readme
Getting started: docs/getting-started.md
Architecture decisions: docs/adr/

License
-------

MIT
