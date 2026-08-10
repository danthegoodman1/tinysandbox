#![cfg(feature = "s3")]

use std::env;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use tinysandbox::sandbox::Sandbox;
use tinysandbox::vfs::{Errno, FileType, OpenMode, S3Vfs, Vfs};

const ALPHA: &[u8] = b"alpha\n";
const NESTED: &[u8] = b"nested needle\nanother line\n";

struct TestConfig {
    endpoint: String,
    region: String,
    access_key: String,
    secret_key: String,
    bucket: String,
    prefix: String,
}

impl TestConfig {
    fn from_env() -> Self {
        let endpoint = required_env("TINYSANDBOX_S3_TEST_ENDPOINT");
        assert_loopback_endpoint(&endpoint);
        Self {
            endpoint,
            region: required_env("TINYSANDBOX_S3_TEST_REGION"),
            access_key: required_env("TINYSANDBOX_S3_TEST_ACCESS_KEY"),
            secret_key: required_env("TINYSANDBOX_S3_TEST_SECRET_KEY"),
            bucket: required_env("TINYSANDBOX_S3_TEST_BUCKET"),
            prefix: required_env("TINYSANDBOX_S3_TEST_PREFIX"),
        }
    }

    fn client(&self) -> Client {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(self.region.clone()))
            .credentials_provider(Credentials::new(
                self.access_key.clone(),
                self.secret_key.clone(),
                None,
                None,
                "tinysandbox-s3-compat-test",
            ))
            .endpoint_url(self.endpoint.clone())
            .force_path_style(true)
            .build();
        Client::from_conf(config)
    }

    fn key(&self, relative: &str) -> String {
        format!("{}/{relative}", self.prefix.trim_matches('/'))
    }
}

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} must be set by scripts/test-s3-compat.sh"))
}

fn assert_loopback_endpoint(endpoint: &str) {
    assert!(
        is_allowed_loopback_endpoint(endpoint),
        "S3 compatibility endpoint must be exactly http://127.0.0.1:<port> or http://localhost:<port>"
    );
}

fn is_allowed_loopback_endpoint(endpoint: &str) -> bool {
    let Some(authority) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "localhost")
        && !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

async fn put(client: &Client, bucket: &str, key: String, data: &[u8]) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .expect("seed local S3 object");
}

fn structured_object() -> Vec<u8> {
    let mut data = b"first record\n".to_vec();
    for index in 0..4096 {
        data.extend_from_slice(format!("record-{index:04} needle payload\n").as_bytes());
    }
    data
}

async fn seed_fixture(config: &TestConfig, client: &Client, large: &[u8]) {
    client
        .create_bucket()
        .bucket(&config.bucket)
        .send()
        .await
        .expect("create unique local S3 bucket");

    for (relative, data) in [
        ("alpha.txt", ALPHA),
        ("nested/data.txt", NESTED),
        ("empty-marker/", b""),
    ] {
        put(client, &config.bucket, config.key(relative), data).await;
    }
    put(client, &config.bucket, config.key("large.txt"), large).await;

    let root = config.prefix.trim_matches('/');
    let (parent, leaf) = root.rsplit_once('/').unwrap_or(("", root));
    let sibling = if parent.is_empty() {
        format!("{leaf}-secret.txt")
    } else {
        format!("{parent}/{leaf}-secret.txt")
    };
    put(client, &config.bucket, sibling, b"must remain invisible\n").await;
}

fn assert_errno<T: std::fmt::Debug>(result: Result<T, tinysandbox::vfs::VfsError>, errno: Errno) {
    assert_eq!(result.expect_err("operation must fail").errno(), errno);
}

#[test]
fn compatibility_endpoint_guard_is_strictly_loopback_only() {
    for endpoint in ["http://127.0.0.1:9000", "http://localhost:1"] {
        assert!(is_allowed_loopback_endpoint(endpoint), "{endpoint}");
    }
    for endpoint in [
        "https://127.0.0.1:9000",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
        "http://127.0.0.1:9000/path",
        "http://localhost:+80",
        "http://localhost.evil:9000",
        "http://s3.amazonaws.com:80",
    ] {
        assert!(!is_allowed_loopback_endpoint(endpoint), "{endpoint}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires scripts/test-s3-compat.sh and its loopback-only MinIO container"]
async fn s3_compatible_adapter_vfs_and_sandbox_end_to_end() {
    let config = TestConfig::from_env();
    let client = config.client();
    let large = structured_object();
    seed_fixture(&config, &client, &large).await;

    let vfs = S3Vfs::new(client.clone(), &config.bucket, Some(&config.prefix))
        .expect("construct prefix-rooted S3 VFS");

    let root = vfs.stat("/").expect("stat virtual root");
    assert_eq!(root.file_type, FileType::Directory);
    assert_eq!(root.len, 0);
    let entries = vfs.readdir("/").expect("list virtual root");
    let actual = entries
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.metadata.file_type,
                entry.metadata.len,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            ("alpha.txt", FileType::File, ALPHA.len() as u64),
            ("empty-marker", FileType::Directory, 0),
            ("large.txt", FileType::File, large.len() as u64),
            ("nested", FileType::Directory, 0),
        ]
    );

    assert_eq!(
        vfs.stat("/nested/data.txt").expect("stat nested file").len,
        NESTED.len() as u64
    );
    assert_eq!(
        vfs.readdir("/nested")
            .expect("list nested directory")
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
        vec!["data.txt"]
    );
    assert!(
        vfs.readdir("/empty-marker")
            .expect("list marker directory")
            .is_empty()
    );
    assert_errno(vfs.stat("/root-secret.txt"), Errno::ENOENT);
    let sibling_name = format!(
        "{}-secret.txt",
        config
            .prefix
            .trim_matches('/')
            .rsplit('/')
            .next()
            .expect("nonempty test prefix")
    );
    let traversal = format!("/../{sibling_name}");
    assert_errno(vfs.stat(&traversal), Errno::ENOENT);
    assert_errno(vfs.open(&traversal, OpenMode::read_only()), Errno::ENOENT);
    assert!(
        vfs.readdir("/")
            .expect("repeat root listing")
            .iter()
            .all(|entry| !entry.name.contains("secret"))
    );

    let handle = vfs
        .open("/large.txt", OpenMode::read_only())
        .expect("open structured object");
    let mut middle = [0; 4096];
    let middle_offset = 65_537;
    assert_eq!(
        vfs.read_at(handle, middle_offset, &mut middle)
            .expect("middle range"),
        middle.len()
    );
    assert_eq!(
        &middle[..],
        &large[middle_offset as usize..middle_offset as usize + middle.len()]
    );
    let mut tail = [0; 64];
    let tail_offset = large.len() as u64 - 7;
    assert_eq!(
        vfs.read_at(handle, tail_offset, &mut tail)
            .expect("tail range"),
        7
    );
    assert_eq!(&tail[..7], &large[large.len() - 7..]);
    assert_eq!(
        vfs.read_at(handle, large.len() as u64, &mut tail)
            .expect("EOF fast path"),
        0
    );
    assert_errno(vfs.write_at(handle, 0, b"no"), Errno::EBADF);
    assert_errno(vfs.truncate(handle, 0), Errno::EINVAL);
    vfs.close(handle).expect("close structured object");
    assert_errno(vfs.read_at(handle, 0, &mut tail), Errno::EBADF);
    assert_errno(vfs.close(handle), Errno::EBADF);

    assert_errno(
        vfs.open("/alpha.txt", OpenMode::write_only()),
        Errno::EACCES,
    );
    assert_errno(
        vfs.open("/new", OpenMode::write_only().create()),
        Errno::EACCES,
    );
    assert_errno(vfs.mkdir("/new-dir"), Errno::EACCES);
    assert_errno(vfs.rename("/alpha.txt", "/renamed"), Errno::EACCES);
    assert_errno(vfs.unlink("/alpha.txt"), Errno::EACCES);
    assert_errno(vfs.rmdir("/nested"), Errno::EACCES);

    let sandbox_vfs =
        S3Vfs::new(client, &config.bucket, Some(&config.prefix)).expect("construct sandbox S3 VFS");
    let sandbox = Sandbox::builder().mount("workspace", sandbox_vfs).build();
    assert_eq!(
        sandbox
            .fs()
            .read_file("/workspace/nested/data.txt")
            .await
            .expect("Sandbox::fs read"),
        NESTED
    );
    let first_fs = sandbox.fs();
    let second_fs = sandbox.fs();
    let (first, second) = tokio::join!(
        first_fs.read_file("/workspace/alpha.txt"),
        second_fs.read_file("/workspace/nested/data.txt")
    );
    assert_eq!(first.expect("first simultaneous read"), ALPHA);
    assert_eq!(second.expect("second simultaneous read"), NESTED);

    let head = sandbox.exec("cat /workspace/large.txt | head -n 1").await;
    assert_eq!(head.exit_code, 0, "{}", head.stderr);
    assert_eq!(head.stdout, "first record\n");
    let grep = sandbox.exec("grep needle /workspace/nested/data.txt").await;
    assert_eq!(grep.exit_code, 0, "{}", grep.stderr);
    assert_eq!(grep.stdout, "nested needle\n");
    let wc = sandbox.exec("wc -c < /workspace/large.txt").await;
    assert_eq!(wc.exit_code, 0, "{}", wc.stderr);
    assert_eq!(wc.stdout.trim(), large.len().to_string());

    let redirect = sandbox.exec("echo forbidden > /workspace/new.txt").await;
    assert_ne!(redirect.exit_code, 0);
    assert!(
        redirect.stderr.contains("Permission denied"),
        "{}",
        redirect.stderr
    );
    let touch = sandbox.exec("touch /workspace/new.txt").await;
    assert_ne!(touch.exit_code, 0);
    assert!(
        touch.stderr.contains("Permission denied"),
        "{}",
        touch.stderr
    );

    #[cfg(feature = "js")]
    {
        let js = sandbox
            .exec(
                r#"js -e "const fs=require('fs'); console.log(fs.readFileSync('/workspace/alpha.txt','utf8').trim())""#,
            )
            .await;
        assert_eq!(js.exit_code, 0, "{}", js.stderr);
        assert_eq!(js.stdout, "alpha\n");
    }
}
