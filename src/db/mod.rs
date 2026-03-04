use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct Database {
    pool: PgPool,
}

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct JustificationRow {
    pub id: String,
    pub avail_chain_id: String,
    pub block_number: i32,
    pub data: serde_json::Value,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;

        Ok(Self { pool })
    }

    pub async fn get_justification(
        &self,
        avail_chain_id: &str,
        block_number: i32,
    ) -> Result<Option<JustificationRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, JustificationRow>(
            r#"
            SELECT id, avail_chain_id, block_number, data, created_at
            FROM justifications
            WHERE avail_chain_id = $1 AND block_number = $2
            "#,
        )
        .bind(avail_chain_id)
        .bind(block_number)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }
}
