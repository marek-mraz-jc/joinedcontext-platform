//! The embedding model and the passages it embeds (T-3053, ADR-N-040 §3.1), with the pinned
//! model files and a real PostgreSQL (see `common`).

mod common;

use assistant::embed::{embed_missing, Embedder, MODEL_FILES};
use assistant::{Error, DIMENSIONS};

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b))
}

#[tokio::test]
async fn a_question_lands_nearest_the_passage_that_answers_it_across_languages() {
    let embedder = Embedder::load(&common::model_dir(), 2).expect("the pinned model loads");
    let passages = embedder
        .passages(&[
            "Knižnica je otvorená v pondelok až piatok od 9:00 do 18:00.".into(),
            "Zmesový odpad vyvážame z rodinných domov každý utorok.".into(),
            "Kaupunkibussit liikennöivät neljällä linjalla arkisin.".into(),
        ])
        .await
        .expect("passages");
    assert_eq!(passages.len(), 3);
    assert!(passages.iter().all(|v| v.len() == DIMENSIONS));
    for (question, answer) in [
        ("Kedy môžem ísť do knižnice?", 0),
        ("Kdy se odváží odpad od rodinných domů?", 1),
        ("When do the town buses run?", 2),
    ] {
        let q = embedder.query(question).await.expect("query");
        let nearest = (0..3)
            .max_by(|&a, &b| cosine(&q, &passages[a]).total_cmp(&cosine(&q, &passages[b])))
            .expect("three passages");
        assert_eq!(nearest, answer, "{question}");
    }
    assert!(embedder.passages(&[]).await.expect("nothing").is_empty());
}

#[test]
fn a_model_file_that_is_not_the_pinned_one_is_refused_before_it_is_loaded() {
    let source = common::model_dir();
    let copy = std::env::temp_dir().join(format!("e5-tampered-{}", std::process::id()));
    std::fs::create_dir_all(&copy).expect("dir");
    for (file, _) in MODEL_FILES {
        std::fs::copy(source.join(file), copy.join(file)).expect("copy");
    }
    std::fs::write(copy.join("tokenizer_config.json"), b"{}").expect("tamper");
    let refused = Embedder::load(&copy, 1).err().expect("refused");
    assert!(matches!(refused, Error::Model(_)));
    let message = refused.to_string();
    assert!(
        message.contains("tokenizer_config.json") && message.contains("e5-small.sh"),
        "{message}"
    );

    std::fs::remove_file(copy.join("config.json")).expect("remove");
    let missing = Embedder::load(&copy, 1).err().expect("refused");
    assert!(
        missing
            .to_string()
            .contains("config.json could not be read"),
        "{missing}"
    );
    std::fs::remove_dir_all(&copy).expect("clean up");
}

#[tokio::test]
async fn missing_embeddings_are_filled_for_the_project_alone_and_a_second_pass_finds_none() {
    let embedder = Embedder::load(&common::model_dir(), 2).expect("the pinned model loads");
    let (admin, pool, name) = common::database("embed").await;
    let texts: Vec<String> = (0..20)
        .map(|i| format!("Odstavec číslo {i} o meste."))
        .collect();
    let pages: Vec<(String, &str, &str)> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| (format!("https://hronov.example/{i}"), "sk", text.as_str()))
        .collect();
    let rows: Vec<(&str, &str, &str)> =
        pages.iter().map(|(u, l, t)| (u.as_str(), *l, *t)).collect();
    common::pages(&pool, "hronov", "web", &rows).await;
    common::pages(
        &pool,
        "lipno",
        "web",
        &[("https://lipno.example/", "cs", "Knihovna je otevřena.")],
    )
    .await;

    // Capped: 7 now, the other 13 on the next call, more than one batch each time.
    assert_eq!(
        embed_missing(&pool, &embedder, "hronov", 7)
            .await
            .expect("first"),
        7
    );
    assert_eq!(
        embed_missing(&pool, &embedder, "hronov", 100)
            .await
            .expect("rest"),
        13
    );
    assert_eq!(
        embed_missing(&pool, &embedder, "hronov", 100)
            .await
            .expect("none"),
        0
    );

    let count = |project: &'static str| {
        let pool = pool.clone();
        async move {
            let mut tx = assistant::project_scope(&pool, project)
                .await
                .expect("scope");
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE embedding IS NULL")
                .fetch_one(&mut *tx)
                .await
                .expect("count");
            n
        }
    };
    assert_eq!(count("hronov").await, 0);
    assert_eq!(
        count("lipno").await,
        1,
        "another project's chunk is not this call's"
    );
    assert!(matches!(
        embed_missing(&pool, &embedder, "Not A Project", 10).await,
        Err(Error::Project(_))
    ));
    common::drop_database(admin, pool, &name).await;
}
