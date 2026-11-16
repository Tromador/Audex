use super::*;
use crate::audex::snapshot::CaptureFile;

#[tokio::test]
async fn native_and_claude_interfaces_navigate_the_same_sealed_file_list() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut files = Vec::new();
    for (name, bytes) in [
        ("client/shared.rs", "needle client\n"),
        ("server/shared.rs", "needle server\n"),
        ("main.rs", "needle main\n"),
    ] {
        let path = temp.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, bytes)?;
        files.push(CaptureFile {
            source_path: AbsolutePathBuf::from_absolute_path(path)?,
            logical_path: SnapshotPath::new(name)?,
        });
    }
    let f = Fixture::with_source(temp, CaptureSource::FileList { files }).await?;
    std::fs::write(f.temp.path().join("server/shared.rs"), "CHANGED LIVE")?;
    let (server_io, client_io) = tokio::io::duplex(128 * 1024);
    let service = SourceService::new(&f.snapshot).with_authority(f.authority().await?);
    let server = tokio::spawn(async move {
        SourceMcpServer::new(service)
            .serve(server_io)
            .await?
            .waiting()
            .await?;
        Ok::<(), anyhow::Error>(())
    });
    let client = ().serve(client_io).await?;
    for (name, args) in [
        ("list", json!({})),
        (
            "read",
            json!({"path":"client/shared.rs","offset":0,"max_bytes":100}),
        ),
        (
            "read",
            json!({"path":"server/shared.rs","offset":0,"max_bytes":100}),
        ),
        ("read", json!({"path":"main.rs","offset":0,"max_bytes":100})),
        (
            "search",
            json!({"query":"needle","path":null,"max_results":10}),
        ),
        (
            "search",
            json!({"query":"needle","path":"server/shared.rs","max_results":10}),
        ),
    ] {
        let native = wrapper(&f.snapshot, false, name, &args.to_string())
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(
            wrapper(&f.snapshot, true, name, &args.to_string())
                .await
                .unwrap(),
            native
        );
        let claude = client
            .call_tool(serde_json::from_value(
                json!({"name":name,"arguments":args}),
            )?)
            .await?;
        assert_ne!(claude.is_error, Some(true));
        assert_eq!(claude.structured_content, Some(native.clone()));
        if name == "read" {
            assert_eq!(native["path"], args["path"]);
            assert!(native["content"].as_str().unwrap().starts_with("needle"));
        } else {
            let text = native.to_string();
            assert!(text.contains("server/shared.rs"));
            if args["path"].is_null() {
                assert!(text.contains("client/shared.rs") && text.contains("main.rs"));
            }
        }
    }
    client.cancel().await?;
    server.await??;
    Ok(())
}
