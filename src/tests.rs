use url::Url;

#[tokio::test]
async fn test_filesystem_read_file() {
    let temp_file = std::env::temp_dir().join("test_read.txt");
    tokio::fs::write(&temp_file, "hello world").await.unwrap();
    let content = tokio::fs::read_to_string(&temp_file).await.unwrap();
    assert_eq!(content, "hello world");
    tokio::fs::remove_file(temp_file).await.unwrap();
}
