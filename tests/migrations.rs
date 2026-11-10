use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// Independent features may land out of migration-number order. Adding a lower version
/// later must still work, provided the binary retains all migrations already applied.
#[sqlx::test(migrations = false)]
async fn a_lower_numbered_migration_can_land_after_a_higher_one(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("0001_base.sql"),
        "CREATE TABLE migration_order (version int PRIMARY KEY);",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("0009_remote.sql"),
        "INSERT INTO migration_order VALUES (9);",
    )
    .unwrap();
    Migrator::new(dir.path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    for version in [7, 8] {
        std::fs::write(
            dir.path().join(format!("{version:04}_feature.sql")),
            format!("INSERT INTO migration_order VALUES ({version});"),
        )
        .unwrap();
    }
    Migrator::new(dir.path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    let versions: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM migration_order ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(versions, [7, 8, 9]);
}
