// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;

use futures::future::BoxFuture;
use semver::Version;
use tracing::info;

use mz_catalog::durable::Transaction;
use mz_ore::collections::CollectionExt;
use mz_ore::now::{EpochMillis, NowFn};
use mz_repr::GlobalId;
use mz_sql::ast::display::AstDisplay;
use mz_sql_parser::ast::{Raw, Statement};
use mz_storage_types::connections::ConnectionContext;

// DO NOT add any more imports from `crate` outside of `crate::catalog`.
use crate::catalog::{Catalog, CatalogState, ConnCatalog};

async fn rewrite_ast_items<F>(tx: &mut Transaction<'_>, mut f: F) -> Result<(), anyhow::Error>
where
    F: for<'a> FnMut(
        &'a mut Transaction<'_>,
        GlobalId,
        &'a mut Statement<Raw>,
    ) -> BoxFuture<'a, Result<(), anyhow::Error>>,
{
    let mut updated_items = BTreeMap::new();
    let items = tx.loaded_items();
    for mut item in items {
        let mut stmt = mz_sql::parse::parse(&item.create_sql)?.into_element().ast;

        f(tx, item.id, &mut stmt).await?;

        item.create_sql = stmt.to_ast_string_stable();

        updated_items.insert(item.id, item);
    }
    tx.update_items(updated_items)?;
    Ok(())
}

async fn rewrite_items<F>(
    tx: &mut Transaction<'_>,
    cat: &ConnCatalog<'_>,
    mut f: F,
) -> Result<(), anyhow::Error>
where
    F: for<'a> FnMut(
        &'a mut Transaction<'_>,
        &'a &ConnCatalog<'_>,
        GlobalId,
        &'a mut Statement<Raw>,
    ) -> BoxFuture<'a, Result<(), anyhow::Error>>,
{
    let mut updated_items = BTreeMap::new();
    let items = tx.loaded_items();
    for mut item in items {
        let mut stmt = mz_sql::parse::parse(&item.create_sql)?.into_element().ast;

        f(tx, &cat, item.id, &mut stmt).await?;

        item.create_sql = stmt.to_ast_string_stable();

        updated_items.insert(item.id, item);
    }
    tx.update_items(updated_items)?;
    Ok(())
}

pub(crate) async fn migrate(
    state: &CatalogState,
    tx: &mut Transaction<'_>,
    _now: NowFn,
    _connection_context: &ConnectionContext,
) -> Result<(), anyhow::Error> {
    let catalog_version = tx.get_catalog_content_version();
    let catalog_version = match catalog_version {
        Some(v) => Version::parse(&v)?,
        None => Version::new(0, 0, 0),
    };

    info!(
        "migrating statements from catalog version {:?}",
        catalog_version
    );

    rewrite_ast_items(tx, |_tx, _id, stmt| {
        let _catalog_version = catalog_version.clone();
        Box::pin(async move {
            // Add per-item AST migrations below.
            //
            // Each migration should be a function that takes `item` (the AST
            // representing the creation SQL for the item) as input. Any
            // mutations to `item` will be staged for commit to the catalog.
            //
            // Migration functions may also take `tx` as input to stage
            // arbitrary changes to the catalog.
            ast_rewrite_create_source_loadgen_options_0_92_0(stmt)?;
            Ok(())
        })
    })
    .await?;

    // Load up a temporary catalog.
    let state = Catalog::load_catalog_items(tx, state)?;

    info!("migrating from catalog version {:?}", catalog_version);

    let conn_cat = state.for_system_session();

    rewrite_items(tx, &conn_cat, |_tx, _conn_cat, _id, _stmt| {
        let _catalog_version = catalog_version.clone();
        Box::pin(async move {
            // Add per-item, post-planning AST migrations below. Most
            // migrations should be in the above `rewrite_ast_items` block.
            //
            // Each migration should be a function that takes `item` (the AST
            // representing the creation SQL for the item) as input. Any
            // mutations to `item` will be staged for commit to the catalog.
            //
            // Be careful if you reference `conn_cat`. Doing so is *weird*,
            // as you'll be rewriting the catalog while looking at it. If
            // possible, make your migration independent of `conn_cat`, and only
            // consider a single item at a time.
            //
            // Migration functions may also take `tx` as input to stage
            // arbitrary changes to the catalog.
            Ok(())
        })
    })
    .await?;

    // Add whole-catalog migrations below.
    //
    // Each migration should be a function that takes `tx` and `conn_cat` as
    // input and stages arbitrary transformations to the catalog on `tx`.
    catalog_clean_up_stash_state_v_0_92_0(tx)?;
    info!(
        "migration from catalog version {:?} complete",
        catalog_version
    );
    Ok(())
}

// Add new migrations below their appropriate heading, and precede them with a
// short summary of the migration's purpose and optional additional commentary
// about safety or approach.
//
// The convention is to name the migration function using snake case:
// > <category>_<description>_<version>
//
// Please include the adapter team on any code reviews that add or edit
// migrations.

// Removes any stash specific state from the catalog.
fn catalog_clean_up_stash_state_v_0_92_0(tx: &mut Transaction) -> Result<(), anyhow::Error> {
    tx.clean_up_stash_catalog()?;
    Ok(())
}

fn _add_to_audit_log(
    tx: &mut Transaction,
    event_type: mz_audit_log::EventType,
    object_type: mz_audit_log::ObjectType,
    details: mz_audit_log::EventDetails,
    occurred_at: EpochMillis,
) -> Result<(), anyhow::Error> {
    let id = tx.get_and_increment_id(mz_catalog::durable::AUDIT_LOG_ID_ALLOC_KEY.to_string())?;
    let event =
        mz_audit_log::VersionedEvent::new(id, event_type, object_type, details, None, occurred_at);
    tx.insert_audit_log_event(event);
    Ok(())
}

fn ast_rewrite_create_source_loadgen_options_0_92_0(
    stmt: &mut Statement<Raw>,
) -> Result<(), anyhow::Error> {
    use mz_sql::ast::visit_mut::VisitMut;
    use mz_sql::ast::{
        CreateSourceConnection, CreateSourceStatement, LoadGenerator, LoadGeneratorOptionName::*,
    };

    struct Rewriter;

    impl<'ast> VisitMut<'ast, Raw> for Rewriter {
        fn visit_create_source_statement_mut(
            &mut self,
            node: &'ast mut CreateSourceStatement<Raw>,
        ) {
            match &mut node.connection {
                CreateSourceConnection::LoadGenerator { generator, options } => {
                    let permitted_options: &[_] = match generator {
                        LoadGenerator::Auction => &[TickInterval],
                        LoadGenerator::Counter => &[TickInterval, MaxCardinality],
                        LoadGenerator::Marketing => &[TickInterval],
                        LoadGenerator::Datums => &[TickInterval],
                        LoadGenerator::Tpch => &[TickInterval, ScaleFactor],
                        LoadGenerator::KeyValue => &[
                            Keys,
                            SnapshotRounds,
                            QuickRounds,
                            ValueSize,
                            UpdateRate,
                            Seed,
                            Partitions,
                            BatchSize,
                        ],
                    };

                    options.retain(|o| permitted_options.contains(&o.name));
                }
                _ => {}
            }
        }
    }

    Rewriter.visit_statement_mut(stmt);

    Ok(())
}
