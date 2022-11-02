use url::Url;

use entities::models::{bootstrap_queue, crawl_queue, indexed_document};
use entities::sea_orm::prelude::*;
use entities::sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use shared::config::LensConfig;

use super::bootstrap;
use super::CrawlTask;
use crate::crawler::Crawler;
use crate::search::Searcher;
use crate::state::AppState;

/// Check if we've already bootstrapped a prefix / otherwise add it to the queue.
#[tracing::instrument(skip(state, lens))]
pub async fn handle_bootstrap(
    state: &AppState,
    lens: &LensConfig,
    seed_url: &str,
    pipeline: Option<String>,
) -> bool {
    let db = &state.db;
    let user_settings = &state.user_settings;

    if let Ok(false) = bootstrap_queue::has_seed_url(db, seed_url).await {
        log::info!("bootstrapping {}", seed_url);

        match bootstrap::bootstrap(lens, db, user_settings, seed_url, pipeline).await {
            Err(e) => {
                log::error!("bootstrap {}", e);
                return false;
            }
            Ok(cnt) => {
                log::info!("bootstrapped {} w/ {} urls", seed_url, cnt);
                let _ = bootstrap_queue::enqueue(db, seed_url, cnt as i64).await;
                return true;
            }
        }
    } else {
        log::info!(
            "bootstrap queue already contains seed url: {}, skipping",
            seed_url
        );
    }

    false
}

#[tracing::instrument(skip(state))]
pub async fn handle_fetch(state: AppState, task: CrawlTask) {
    let crawler = Crawler::new();
    let result = crawler.fetch_by_job(&state, task.id, true).await;

    match result {
        Ok(Some(crawl_result)) => {
            // Update job status
            // We consider 400s complete in this case since we manage to hit the server
            // successfully but nothing useful was returned.
            let cq_status = if crawl_result.is_success() || crawl_result.is_bad_request() {
                crawl_queue::CrawlStatus::Completed
            } else {
                crawl_queue::CrawlStatus::Failed
            };

            let _ = crawl_queue::mark_done(&state.db, task.id, cq_status).await;

            // Add all valid, non-duplicate, non-indexed links found to crawl queue
            let to_enqueue: Vec<String> = crawl_result.links.into_iter().collect();

            // Collect all lenses that do not
            let lenses: Vec<LensConfig> = state
                .lenses
                .iter()
                .filter(|entry| entry.value().pipeline.is_none())
                .map(|entry| entry.value().clone())
                .collect();

            if let Err(err) = crawl_queue::enqueue_all(
                &state.db,
                &to_enqueue,
                &lenses,
                &state.user_settings,
                &Default::default(),
                Option::None,
            )
            .await
            {
                log::error!("error enqueuing all: {}", err);
            }

            // Only add valid urls
            // if added.is_none() || added.unwrap() == crawl_queue::SkipReason::Duplicate {
            //     link::save_link(&state.db, &crawl_result.url, link)
            //         .await
            //         .unwrap();
            // }

            // Add / update search index w/ crawl result.
            if let Some(content) = crawl_result.content {
                let url = Url::parse(&crawl_result.url).expect("Invalid crawl URL");
                let url_host = url.host_str().expect("Invalid URL host");

                let existing = indexed_document::Entity::find()
                    .filter(indexed_document::Column::Url.eq(url.as_str()))
                    .one(&state.db)
                    .await
                    .unwrap_or_default();

                // Delete old document, if any.
                if let Some(doc) = &existing {
                    if let Ok(mut index_writer) = state.index.writer.lock() {
                        let _ = Searcher::remove_from_index(&mut index_writer, &doc.doc_id);
                    }
                }

                // Add document to index
                let doc_id: Option<String> = {
                    if let Ok(mut index_writer) = state.index.writer.lock() {
                        match Searcher::add_document(
                            &mut index_writer,
                            &crawl_result.title.unwrap_or_default(),
                            &crawl_result.description.unwrap_or_default(),
                            url_host,
                            url.as_str(),
                            &content,
                            &crawl_result.raw.unwrap_or_default(),
                        ) {
                            Ok(new_doc_id) => Some(new_doc_id),
                            _ => None,
                        }
                    } else {
                        None
                    }
                };

                if let Some(doc_id) = doc_id {
                    // Update/create index reference in our database
                    let indexed = if let Some(doc) = existing {
                        let mut update: indexed_document::ActiveModel = doc.into();
                        update.doc_id = Set(doc_id);
                        update.open_url = Set(crawl_result.open_url);
                        update
                    } else {
                        indexed_document::ActiveModel {
                            domain: Set(url_host.to_string()),
                            url: Set(url.as_str().to_string()),
                            open_url: Set(crawl_result.open_url),
                            doc_id: Set(doc_id),
                            ..Default::default()
                        }
                    };

                    if let Err(e) = indexed.save(&state.db).await {
                        log::error!("Unable to save document: {}", e);
                    }
                }
            }
        }
        Ok(None) => {
            // Failed to grab robots.txt or crawling is not allowed
            if let Err(e) =
                crawl_queue::mark_done(&state.db, task.id, crawl_queue::CrawlStatus::Completed)
                    .await
            {
                log::error!("Unable to mark task as finished: {}", e);
            }
        }
        Err(err) => {
            log::error!("Unable to crawl id: {} - {:?}", task.id, err);
            // mark crawl as failed
            if let Err(e) =
                crawl_queue::mark_done(&state.db, task.id, crawl_queue::CrawlStatus::Failed).await
            {
                log::error!("Unable to mark task as failed: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use entities::models::bootstrap_queue;
    use entities::test::setup_test_db;

    use super::{handle_bootstrap, AppState};
    use crate::search::IndexPath;
    use shared::config::UserSettings;

    #[tokio::test]
    async fn test_handle_bootstrap() {
        let db = setup_test_db().await;
        let state = AppState::builder()
            .with_db(db)
            .with_user_settings(&UserSettings::default())
            .with_index(&IndexPath::Memory)
            .build();

        let test = "https://example.com";

        bootstrap_queue::enqueue(&state.db, test, 10).await.unwrap();
        assert!(!handle_bootstrap(&state, &Default::default(), &test, None).await);
    }
}