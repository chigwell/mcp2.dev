<div align="center">
  <a target="_blank" href="https://mcp2.dev/?utm_source=github&utm_medium=readme">
   <img src="https://capsule-render.vercel.app/api?type=waving&color=gradient&height=200&section=header&text=MCP2.dev&fontSize=50&fontAlignY=35&animation=fadeIn&fontColor=FFFFFF&descAlignY=55&descAlign=62" alt="Telegram MCP Server" width="100%" />
  </a>
</div>

`mcp2.dev` lets you expose your locally running web server via a public URL.
Written in Rust. Built completely with async-io on top of tokio.

1. [Install](#install)
2. [Usage Instructions](#usage)
3. [Host it yourself](#host-it-yourself)

# Install
## Brew (macOS)
```bash
brew install chigwell/tap/mcp2dev
```

## Cargo
```bash
cargo install mcp2dev
```

## Everywhere
Or **Download a release for your target OS here**: [mcp2dev/releases](https://github.com/chigwell/mcp2.dev/releases)

# Usage
## Quick Start
```shell script
mcp2dev --port 8000
```
The above command opens a tunnel and forwards traffic to `localhost:8000`.

## More Options:
```shell script
mcp2dev 0.1.14

USAGE:
    mcp2dev [FLAGS] [OPTIONS] [SUBCOMMAND]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information
    -v, --verbose    A level of verbosity, and can be used multiple times

OPTIONS:
        --dashboard-address <dashboard-address>    Sets the address of the local introspection dashboard
    -k, --key <key>                                Sets an API authentication key to use for this tunnel
        --host <local-host>
            Sets the HOST (i.e. localhost) to forward incoming tunnel traffic to [default: localhost]

    -p, --port <port>
            Sets the port to forward incoming tunnel traffic to on the target host

        --scheme <scheme>
            Sets the SCHEME (i.e. http or https) to forward incoming tunnel traffic to [default: http]

    -s, --subdomain <sub-domain>                   Specify a sub-domain for this tunnel

SUBCOMMANDS:
    help        Prints this message or the help of the given subcommand(s)
    set-auth    Store the API Authentication key
```

# Host it yourself
1. Compile the server for the musl target. See the `musl_build.sh` for a way to do this trivially with Docker!
2. See `Dockerfile` for a simple alpine based image that runs that server binary.
3. Deploy the image where ever you want.

## Testing Locally
```shell script
# Run the Server: xpects TCP traffic on 8080 and control websockets on 5000
ALLOWED_HOSTS="localhost" ALLOW_ANON=1 cargo run --bin tunnelto_server

# Run a local mcp2dev client talking to your local tunnelto_server
CTRL_HOST="localhost" CTRL_PORT=5000 CTRL_TLS_OFF=1 cargo run --bin mcp2dev -- -p 8000

# Test it out!
# Remember 8080 is our local mcp2dev TCP server
curl -H '<subdomain>.localhost' "http://localhost:8080/some_path?with=somequery"
```
See `tunnelto_server/src/config.rs` for the environment variables for configuration.

## Caveats for hosting it yourself
The implementation does not support multiple running servers (i.e. centralized coordination).
Therefore, if you deploy multiple instances of the server, it will only work if the client connects to the same instance
as the remote TCP stream.

The [version hosted by us](https://mcp2.dev) is a proper distributed system running on the the fabulous [fly.io](https://fly.io) service. 
In short, fly.io makes this super easy with their [Private Networking](https://fly.io/docs/reference/privatenetwork/) feature.
See `tunnelto_server/src/network/mod.rs` for the implementation details of our gossip mechanism.
