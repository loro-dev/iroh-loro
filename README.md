# iroh-loro demo

A peer-to-peer collaborative text editing demo that uses
[iroh](https://github.com/n0-computer/iroh) for p2p networking and
[loro](https://github.com/loro-dev/loro) for CRDT-based text synchronization.

> This demo only works for 2 peers and a single text file at the moment

## Usage

The demo supports two modes: serving a file (hosting) and joining an existing
session.

### Hosting a File

To start hosting a file for collaborative editing:

```bash
cargo run -r serve <FILE_PATH>
```

This will:

1. Start watching the specified file for changes
2. Print your Node ID that others can use to connect
3. Automatically sync changes on the given text file with any peers that join

### Joining a Session

To join an existing collaborative editing session:

```bash
cargo run -r join <REMOTE_NODE_ID> <LOCAL_FILE_PATH>
```

Where:

- `REMOTE_NODE_ID` is the Node ID printed by the host
- `LOCAL_FILE_PATH` is where you want to save the synchronized file locally
