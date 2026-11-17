use super::{
    test_http::{self, Response},
    RawIo, RemoteStore,
};
use crate::sync::types::Manifest;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[tokio::test]
async fn http_backend_contracts() {
    for dav in [false, true] {
        let files = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let omit_etag = Arc::new(AtomicBool::new(false));
        let missing_tag = omit_etag.clone();
        let state = files.clone();
        let denied = Arc::new(AtomicBool::new(false));
        let deny = denied.clone();
        let server=test_http::serve(move|request|{
            if deny.load(Ordering::SeqCst){return Response::new(403,"denied");}
            let key=request.path.split('?').next().unwrap().to_string();
            let mut files=state.lock().unwrap();
            match request.method.as_str() {
                "PUT"=>{files.insert(key,request.body);Response::new(200,"")},
                "GET"=>match files.get(&key){Some(body)=>Response::new(200,body.clone()),None=>Response::new(404,"<Error><Code>NoSuchKey</Code></Error>")},
                "DELETE"=>{files.remove(&key);Response::new(204,"")},
                "MKCOL"=>Response::new(201,""),
                "HEAD"=>match files.get(&key){Some(body)=>{let mut r=Response::new(200,body.clone());if !missing_tag.load(Ordering::SeqCst){r.headers.push(("ETag".into(),format!("\"{}\"",body.len())));}r},None=>Response::new(404,"")},
                "PROPFIND"=>{
                    let tag=if missing_tag.load(Ordering::SeqCst){""}else{"<d:getetag>v1</d:getetag>"};
                    let body=format!(r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>{key}manifest.json</d:href><d:propstat><d:prop><d:getcontentlength>10</d:getcontentlength>{tag}<d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#);
                    Response::new(207,body)
                },
                _=>Response::new(500,"unexpected operation"),
            }
        }).await;
        let store: Box<dyn RemoteStore> = if dav {
            Box::new(super::webdav::WebDavStore::new(
                server.url.clone(),
                "u".into(),
                "p".into(),
                "root".into(),
            ))
        } else {
            Box::new(
                super::s3::S3Store::new(
                    server.url.clone(),
                    "us-east-1".into(),
                    "bucket".into(),
                    "key".into(),
                    "secret".into(),
                    true,
                    "root".into(),
                )
                .await,
            )
        };
        let tmp = std::env::temp_dir().join(format!("synccloud-contract-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let source = tmp.join("source");
        let dest = tmp.join("dest");
        std::fs::write(&source, b"hello").unwrap();
        store.put_file("nested/a#b?.txt", &source).await.unwrap();
        store.get_file("nested/a#b?.txt", &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        std::fs::write(&source, b"replacement").unwrap();
        store.put_file("nested/a#b?.txt", &source).await.unwrap();
        store.get_file("nested/a#b?.txt", &dest).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"replacement");
        let mut manifest = Manifest::new("fixture");
        store.write_manifest(&manifest).await.unwrap();
        assert!(store.read_manifest().await.unwrap().is_some());
        assert!(store.manifest_stamp().await.unwrap().is_some());
        omit_etag.store(true, Ordering::SeqCst);
        manifest.seq = 1;
        store.write_manifest(&manifest).await.unwrap();
        let before = store.manifest_stamp().await.unwrap();
        manifest.seq = 2;
        store.write_manifest(&manifest).await.unwrap();
        assert_ne!(
            before,
            store.manifest_stamp().await.unwrap(),
            "missing ETag must still detect equal-length changes"
        );
        store.delete_file("nested/a#b?.txt").await.unwrap();
        assert!(store.get_file("nested/a#b?.txt", &dest).await.is_err());
        assert!(store.put_file("../escape", &source).await.is_err());
        denied.store(true, Ordering::SeqCst);
        assert!(store.put_file("a", &source).await.is_err());
        assert!(store.delete_file("a").await.is_err());
        assert!(store.raw_io().read_text("locks/test").await.is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }
}

#[tokio::test]
async fn git_contract_lock_and_failed_push_recovery() {
    let tmp = std::env::temp_dir().join(format!("synccloud-git-contract-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let remote = tmp.join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&remote)
        .output()
        .unwrap()
        .status
        .success());
    let cache = tmp.join("local");
    let store = super::git_backend::GitStore::new(
        remote.to_string_lossy().into(),
        "synccloud".into(),
        "".into(),
        "".into(),
    )
    .with_cache_dir(cache.clone());
    store
        .put_text("locks/fixture", "fixture|9999999999999")
        .await
        .unwrap();
    assert_eq!(
        store.read_text("locks/fixture").await.unwrap().as_deref(),
        Some("fixture|9999999999999")
    );
    store.delete("locks/fixture").await.unwrap();
    assert!(store.read_text("locks/fixture").await.unwrap().is_none());
    let src = tmp.join("src");
    let dest = tmp.join("dest");
    std::fs::write(&src, b"original").unwrap();
    store.put_file("a.txt", &src).await.unwrap();
    let mut manifest = Manifest::new("fixture");
    manifest.seq = 1;
    let hook = remote.join("hooks/pre-receive");
    std::fs::write(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
    assert!(store.write_manifest(&manifest).await.is_err());
    std::fs::remove_file(hook).unwrap();
    store.write_manifest(&manifest).await.unwrap();
    store.get_file("a.txt", &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
    store.delete_file("a.txt").await.unwrap();
    // A process restart must retain the deletion intent.
    drop(store);
    let store = super::git_backend::GitStore::new(
        remote.to_string_lossy().into(),
        "synccloud".into(),
        "".into(),
        "".into(),
    )
    .with_cache_dir(cache);
    store.write_manifest(&manifest).await.unwrap();
    assert!(store.get_file("a.txt", &dest).await.is_err());
    assert!(
        store.raw_io().read_text("../outside").await.is_err(),
        "invalid metadata paths must be rejected"
    );
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&remote)
        .args(["update-ref", "-d", "refs/heads/synccloud-meta/synccloud"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(
        store.read_manifest().await.unwrap().is_none(),
        "removed remote metadata branch must not read stale cached ref"
    );
    std::fs::remove_dir_all(tmp).unwrap();
}
