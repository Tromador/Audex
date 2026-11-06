//! Delivery limits are shared by native and MCP execution, before capture.
use super::*;

#[test]
fn encoded_result_boundary_is_explicit_and_does_not_clamp() -> anyhow::Result<()> {
    for size in [MAX_RESULT_BYTES - 1, MAX_RESULT_BYTES, MAX_RESULT_BYTES + 1] {
        // Two JSON quotes are part of the delivery budget.
        let value = json!("x".repeat(size - 2));
        assert_eq!(value.to_string().len(), size);
        let result = bound_result(value.clone());
        if size <= MAX_RESULT_BYTES {
            assert_eq!(result?, value);
        } else {
            assert_eq!(result.unwrap_err().to_string(), RESULT_LIMIT_MESSAGE);
            assert_eq!(
                diagnostics::failure(Some(RESULT_LIMIT_MESSAGE))["detail"],
                "available"
            );
            assert_eq!(
                diagnostics::failure(Some(&format!("{RESULT_LIMIT_MESSAGE}: /private")))["detail"],
                "unavailable"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn maximum_reads_expansion_search_limits_and_mcp_metadata_match() -> anyhow::Result<()> {
    let path = std::iter::repeat_n("\"".repeat(200), 15)
        .collect::<Vec<_>>()
        .join("/");
    let bytes = vec![0; 65536];
    let f = Fixture::with_path(&path, &bytes).await?;
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
    for tool in client.list_all_tools().await? {
        let advertised = serde_json::to_value(tool)?;
        assert_eq!(
            advertised["_meta"]["anthropic/maxResultSizeChars"],
            MAX_RESULT_BYTES
        );
    }
    for (name, args, succeeds) in [
        ("list", json!({}), true),
        (
            "read",
            json!({"path":path,"offset":0,"max_bytes":65536}),
            true,
        ),
        (
            "read",
            json!({"path":path,"offset":65535,"max_bytes":65536}),
            true,
        ),
        (
            "read",
            json!({"path":path,"offset":65536,"max_bytes":1}),
            true,
        ),
        (
            "search",
            json!({"query":"\u{0000}","path":path,"max_results":100}),
            false,
        ),
        (
            "search",
            json!({"query":"\u{0000}","path":path,"max_results":50}),
            true,
        ),
    ] {
        let native = wrapper(&f.snapshot, false, name, &args.to_string()).await;
        assert_eq!(
            native,
            wrapper(&f.snapshot, true, name, &args.to_string()).await
        );
        let reply = client
            .call_tool(serde_json::from_value(
                json!({"name":name,"arguments":args}),
            )?)
            .await?;
        if succeeds {
            let expected = native.map_err(anyhow::Error::msg)?;
            assert_ne!(reply.is_error, Some(true));
            assert_eq!(reply.structured_content, Some(expected.clone()));
            assert_eq!(
                serde_json::from_str::<Value>(
                    serde_json::to_value(reply.content)?[0]["text"]
                        .as_str()
                        .unwrap()
                )?,
                expected
            );
            assert!(expected.to_string().len() <= MAX_RESULT_BYTES);
            if name == "read" {
                assert_eq!(expected["eof"], true);
                assert_eq!(expected["end_offset"], 65536);
                assert_eq!(expected["sha256"], sha256_hex(&bytes));
                assert_eq!(
                    expected["content"].as_str().unwrap().as_bytes(),
                    &bytes[args["offset"].as_u64().unwrap() as usize..]
                );
            }
        } else {
            assert_eq!(native.unwrap_err(), RESULT_LIMIT_MESSAGE);
            assert_eq!(reply.is_error, Some(true));
            assert_eq!(
                serde_json::to_value(reply.content)?[0]["text"],
                RESULT_LIMIT_MESSAGE
            );
        }
    }
    // A multi-entry list is an injected transport stress case, not a valid v1 snapshot.
    let mut manifest = f.snapshot.manifest().clone();
    manifest
        .identity
        .entries
        .push(manifest.identity.entries[0].clone());
    assert!(manifest.validate().is_err());
    client.cancel().await?;
    server.await??;
    Ok(())
}
