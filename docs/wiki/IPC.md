You can communicate with the running tiri instance over an IPC socket.
Check `tiri msg --help` for available commands.

The `--json` flag prints the response in JSON, rather than formatted.
For example, `tiri msg --json outputs`.

> [!TIP]
> If you're getting parsing errors from `tiri msg` after upgrading tiri, make sure that you've restarted tiri itself.
> You might be trying to run a newer `tiri msg` against an older `tiri` compositor.

### Event Stream

<sup>Upstream niri: 0.1.9</sup>

While most tiri IPC requests return a single response, the event stream request will make tiri continuously stream events into the IPC connection until it is closed.
This is useful for implementing various bars and indicators that update as soon as something happens, without continuous polling.

The event stream IPC is designed to give you the complete current state up-front, then follow up with updates to that state.
This way, your state can never "desync" from tiri, and you don't need to make any other IPC information requests.

Where reasonable, event stream state updates are atomic, though this is not always the case.
For example, a window may end up with a workspace id for a workspace that had already been removed.
This can happen if the corresponding workspaces-changed event arrives before the corresponding window-changed event.

To get a taste of the events, run `tiri msg event-stream`.
Though, this is more of a debug function than anything.
You can get raw events from `tiri msg --json event-stream`, or by connecting to the tiri socket and requesting an event stream manually.

You can find the full list of events along with documentation in upstream niri's IPC docs [here](https://yalter.github.io/niri/niri_ipc/enum.Event.html).

### Programmatic Access

`tiri msg --json` is a thin wrapper over writing and reading to a socket.
When implementing more complex scripts and modules, you're encouraged to access the socket directly.

Connect to the UNIX domain socket located at `$TIRI_SOCKET` in the filesystem.
Write your request encoded in JSON on a single line, followed by a newline character, or by flushing and shutting down the write end of the connection.
Read the reply as JSON, also on a single line.

You can use `socat` to test communicating with tiri directly:

```sh
$ socat STDIO "$TIRI_SOCKET"
"FocusedWindow"
{"Ok":{"FocusedWindow":{"id":12,"title":"t socat STDIO /run/u ~","app_id":"Alacritty","workspace_id":6,"is_focused":true}}}
```

The reply is an `Ok` or an `Err` wrapping the same JSON object as you get from `tiri msg --json`.

<sup>Upstream niri: next release</sup>
For more complex requests, you can pass `--print-request` to `tiri msg` to see how the request should be formatted:

```sh
$ tiri msg --print-request action focus-workspace 2
{"Action":{"FocusWorkspace":{"reference":{"Index":2}}}}
```

This is the format that you should use for communicating with the tiri socket directly.
You can also use `tiri msg raw-request` to send a raw JSON request without `socat`:

```sh
$ echo '{"Action":{"FocusWorkspace":{"reference":{"Id":8}}}}' | tiri msg raw-request
"Handled"
```

You can find all available requests and response types in upstream niri's [`niri-ipc` sub-crate documentation](https://niri-wm.github.io/niri/niri_ipc/).

### Backwards Compatibility

The JSON output *should* remain stable, as in:

- existing fields and enum variants should not be renamed
- non-optional existing fields should not be removed

However, new fields and enum variants will be added, so you should handle unknown fields or variants gracefully where reasonable.

The formatted/human-readable output (i.e. without `--json` flag) is **not** considered stable.
Please prefer the JSON output for scripts, since I reserve the right to make any changes to the human-readable output.

The `tiri-ipc` sub-crate (like other tiri sub-crates) is *not* API-stable in terms of the Rust semver; rather, it follows the version of tiri itself.
In particular, new struct fields and enum variants will be added.
