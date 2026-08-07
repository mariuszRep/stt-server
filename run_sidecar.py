import argparse
import sys

from app import cli


def main() -> int:
    parser = argparse.ArgumentParser(prog="stt-server")
    sub = parser.add_subparsers(dest="command")

    serve_p = sub.add_parser("serve", help="Run the server in the foreground (default).")
    serve_p.add_argument("--log-file", default=None, help="Redirect stdout/stderr to this file.")
    serve_p.set_defaults(func=cli.cmd_serve)

    install_p = sub.add_parser("install", help="Install the binary and register it to start at login.")
    install_p.set_defaults(func=cli.cmd_install)

    uninstall_p = sub.add_parser("uninstall", help="Stop, unregister auto-start, and remove the installed binary.")
    uninstall_p.set_defaults(func=cli.cmd_uninstall)

    start_p = sub.add_parser("start", help="Start the server in the background.")
    start_p.set_defaults(func=cli.cmd_start)

    stop_p = sub.add_parser("stop", help="Stop the running server.")
    stop_p.set_defaults(func=cli.cmd_stop)

    status_p = sub.add_parser("status", help="Report whether the server is running and healthy.")
    status_p.set_defaults(func=cli.cmd_status)

    logs_p = sub.add_parser("logs", help="Print the server's log file.")
    logs_p.add_argument("-n", "--lines", type=int, default=200, help="Number of trailing lines (0 = all).")
    logs_p.set_defaults(func=cli.cmd_logs)

    args = parser.parse_args()

    # No subcommand: today's original, argument-free behavior — this is the
    # exact invocation Tauri's sidecar spawn uses (whisper-vibes,
    # apps/desktop/src-tauri), so it must stay untouched.
    if args.command is None:
        args.log_file = None
        return cli.cmd_serve(args)

    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
