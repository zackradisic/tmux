//! Tiny fs API probe: async write -> async read -> sync write -> sync
//! read, logging each step. Load with -c fs-read -c fs-write.

use tmux_plugin_sdk::prelude::*;

struct FsProbe;

impl Plugin for FsProbe {
    const NAME: &'static str = "fs-probe";
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        ctx.spawn(async {
            match fs_write("probe/hello.txt", b"hello fs".to_vec(), false)
                .await
            {
                Ok(n) => log(&format!("fs_write ok: {n} bytes")),
                Err(e) => {
                    log(&format!("fs_write failed: {e}"));
                    return;
                }
            }
            match fs_write("probe/hello.txt", b" + more".to_vec(), true).await
            {
                Ok(n) => log(&format!("fs_append ok: {n} bytes")),
                Err(e) => log(&format!("fs_append failed: {e}")),
            }
            match fs_read("probe/hello.txt", 0, 64).await {
                Ok((data, eof)) => log(&format!(
                    "fs_read ok (eof {eof}): {:?}",
                    String::from_utf8_lossy(&data)
                )),
                Err(e) => log(&format!("fs_read failed: {e}")),
            }
            match fs_write_sync("probe/sync.txt", b"sync bytes", false) {
                Ok(n) => log(&format!("fs_write_sync ok: {n} bytes")),
                Err(e) => log(&format!("fs_write_sync failed: {e}")),
            }
            let mut buf = Vec::with_capacity(64);
            match fs_read_sync("probe/sync.txt", 5, &mut buf) {
                Ok(eof) => log(&format!(
                    "fs_read_sync ok (eof {eof}): {:?}",
                    String::from_utf8_lossy(&buf)
                )),
                Err(e) => log(&format!("fs_read_sync failed: {e}")),
            }
            // Sandbox: this must fail.
            match fs_write("../escape.txt", b"x".to_vec(), false).await {
                Ok(_) => log("SANDBOX HOLE: ../escape.txt written"),
                Err(e) => log(&format!("sandbox ok: {e}")),
            }
        });
        Ok(Self)
    }
}

tmux_plugin!(FsProbe);
