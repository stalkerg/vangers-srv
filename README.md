## Vangers-Srv

Vangers-Srv is a reimplemented of the Vangers Server published by KranX. Its written in pure Rust without any unsafe features and uses tokio as async runtime.

Vangers-Srv is under active development and, unfortunately, is not full-compatible with original protocol, at least, for now. Additionally, the server hasn't all features that implemented by original server (for example, total player ratings are missed).
However, the server is playable and you can play with your friends using any Vangers game client (Steam, GOG or your own compiled from source).

`Vangers-Srv` running on `fenex.vangers.net:2197`.

## See also
* [Vangers](https://github.com/KranX/Vangers/) - original Vangers game & server
* [vange-rs](https://github.com/kvark/vange-rs) - Vangers game clone written in Rust

### Negotiated UTF-8 player text

Protocol 6 remains the default as before. Before attach/restore, a client may
offer `VANGERS-UTF8/1` as the initial server-time query body. Only an exact
echo after the four clock bytes opts that socket into UTF-8 name and chat
fields. Old clients receive plain time responses and CP866 text. In mixed
games the server renders each receiver's wire representation independently;
unrepresentable legacy names/messages have explicit ASCII aliases/notices.
Game IDs, player IDs, masks, passwords and non-text protocol are unchanged.

After building this server and the preceding version, run the loopback matrix:

```sh
python3 tests/utf8_wire.py /path/to/updated/vangers-srv /path/to/previous/vangers-srv
```
