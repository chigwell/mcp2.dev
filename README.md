<div align="center">
  <a target="_blank" href="https://mcp2.dev/?utm_source=github&utm_medium=readme">
   <img src="https://capsule-render.vercel.app/api?type=waving&color=gradient&height=200&section=header&text=MCP2.dev&fontSize=50&fontAlignY=35&animation=fadeIn&fontColor=FFFFFF&descAlignY=55&descAlign=62" alt="Telegram MCP Server" width="100%" />
  </a>
</div>

`mcp2.dev` lets you expose your locally running web server via a public URL.
Written in Rust. Built completely with async-io on top of tokio.

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
curl "http://localhost:8080/<tunnel_id>/some_path?with=somequery"
```
See `tunnelto_server/src/config.rs` for the environment variables for configuration.

## Caveats for hosting it yourself
The implementation does not support multiple running servers (i.e. centralized coordination).
Therefore, if you deploy multiple instances of the server, it will only work if the client connects to the same instance
as the remote TCP stream.

The [version hosted by us](https://mcp2.dev) is a proper distributed system running on the the fabulous [fly.io](https://fly.io) service. 
In short, fly.io makes this super easy with their [Private Networking](https://fly.io/docs/reference/privatenetwork/) feature.
See `tunnelto_server/src/network/mod.rs` for the implementation details of our gossip mechanism.
