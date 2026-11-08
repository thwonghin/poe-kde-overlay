use std::net::TcpStream;
use std::path::PathBuf;

use md5::{Digest, Md5};
use serde_json::json;

use crate::game::Game;
use crate::server::write_response;

fn uploads_dir(game: Game) -> PathBuf {
    game.data_dir().join("uploads")
}

pub fn handle(game: Game, stream: &mut TcpStream, method: &str, path: &str, body: &[u8]) -> std::io::Result<()> {
    let name = path.strip_prefix("/uploads/").unwrap_or("");

    if method == "GET" {
        let Some(file) = crate::server::safe_join(&uploads_dir(game), name) else {
            return write_response(stream, 404, "text/plain", b"not found");
        };
        match std::fs::read(file) {
            Ok(bytes) => write_response(stream, 200, "", &bytes),
            Err(_) => write_response(stream, 404, "text/plain", b"not found"),
        }
    } else if method == "POST" {
        let dir = uploads_dir(game);
        std::fs::create_dir_all(&dir)?;
        let hash = Md5::digest(body)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let filename = format!("{hash}{ext}");
        std::fs::write(dir.join(&filename), body)?;
        let response = json!({ "name": filename }).to_string();
        write_response(stream, 200, "application/json", response.as_bytes())
    } else {
        write_response(stream, 400, "text/plain", b"unsupported method")
    }
}
