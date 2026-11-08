# Installing iphone-use (for AI agents)

These are the steps an AI agent follows to install iphone-use on the user's Mac and register its MCP
server. iphone-use is a macOS app with a daemon, not an npm or pip package: there is nothing to
`npx`, and the MCP server only works on the Mac the iPhone is plugged into.

## Requirements (check before you start)

- macOS 15 or later (`sw_vers -productVersion`). On any other OS, stop and tell the user.
- Xcode installed and signed in to an Apple ID (a free one works). Only the user can sign in.
- An iPhone with iOS 15 or later, Developer Mode on, plugged in by USB and unlocked for the first setup.

## Steps

1. Install. Run in a terminal on the Mac:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
   ```

   The installer puts the app at `~/Applications/iPhoneUse.app`, the CLI on the PATH as
   `iphone-use`, sets the phone up when one is plugged in, and registers the MCP server with
   Claude Code (`claude mcp add-json -s user iphone-use …`) and Codex (`codex mcp add iphone-use …`)
   when those CLIs are present.

2. Diagnose. Run `iphone-use doctor`. Fix what it reports that can be fixed from the terminal, and run
   `iphone-use setup` when it tells you to.

3. Hand the user what only they can do: signing in to an Apple ID in Xcode, tapping Trust on the
   iPhone, turning on Developer Mode, unlocking the phone. Stop, tell them exactly what to do, and wait
   until they say it is done. Never ask for a password, a passcode or a verification code.

4. Repeat `iphone-use doctor` until nothing is missing, then run `iphone-use try` (opens Settings,
   reads the screen, goes Home).

5. Register the MCP server with any other client. The command is the same everywhere and needs no
   arguments, env or token; on this Mac it finds the daemon and its token by itself:

   ```text
   /Users/<user>/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp
   ```

   Use the absolute path (expand `$HOME`; most JSON configs do not). For example, for a client that
   reads an `mcpServers` map (Claude Desktop, Cursor, Windsurf, Gemini CLI):

   ```json
   {
     "mcpServers": {
       "iphone-use": {
         "command": "/Users/<user>/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
       }
     }
   }
   ```

   Merge this entry into the client's existing file; do not replace other servers. The README's
   "Use it from an agent" section has the file location and format for each client.

6. If the `phone_*` tools are not loaded in the current chat, tell the user to start a new chat (or
   restart the client) so it picks up the server.

## Options

- **A second phone.** One daemon drives one phone. For another iPhone, install a named instance
   and leave the default alone: `curl -fsSL …/install.sh | sh -s -- --instance lab --udid <UDID>`
   (UDID from `xcrun devicectl list devices`), then `iphone-use setup --instance lab`. The
   instance gets its own daemon port and token; the installer prints them. Point a client at it
   with `IPHONE_USE_INSTANCE=lab` in the MCP server's `env`, or pass `instance: "lab"` on each
   `phone_*` call (`phone_instances` lists them). `doctor`, `status`, `metrics`, `test` and
   `schedule` take `--instance lab`.
- **Control from another network.** `iphone-use remote tailscale on` joins the daemon to the
   user's Tailscale tailnet as its own node; `iphone-use remote cloudflare on` publishes it through
   a Cloudflare quick tunnel (no account; `--token-file <file> --url https://phone.example.com` for a
   named tunnel on the user's domain). `iphone-use remote status` shows the addresses, `iphone-use
   remote pair` prints a one-time QR code and link for the phone app. Both still need the daemon
   password and survive upgrades. Only the user can sign in to Tailscale or Cloudflare; stop and
   ask them.
- **Fewer tools.** `IPHONE_USE_MCP_TOOLS` in the server's `env` narrows what the model sees:
   `core` (everything except the developer tools: logs, crashes, recording, app install,
   simulated location, app notes), `readonly` (observation and lookup only; nothing acts on the
   phone) or a comma list of tool names. A hidden tool is neither listed nor callable.
- **MCP over HTTP.** `iphone-use-mcp --listen 127.0.0.1:PORT` serves MCP over HTTP for a client
   on the same Mac. For another address set `IPHONE_USE_MCP_TOKEN` (required off loopback) and
   have the client send it as `Authorization: Bearer <token>`; prefer an authenticated tunnel over
   exposing the port.

## Check that it works

- `iphone-use status` says whether the phone is ready for agents.
- In the client, call `phone_status`, then `phone_elements` to read the screen.

## Undo

`~/.iphone-use/uninstall.sh --dry-run` lists what uninstalling removes; drop `--dry-run` to do it.
