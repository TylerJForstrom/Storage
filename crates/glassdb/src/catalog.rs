//! The catalog: where table definitions live.
//!
//! GlassDB eats its own dog food — the catalog is just another B+tree
//! (rooted at the page recorded in the header), keyed by table id, whose
//! values are encoded `TableSchema`s. SQLite does the same thing with its
//! `sqlite_schema` table.

use std::ops::Bound;

use crate::btree::BTree;
use crate::errors::DbResult;
use crate::pager::Pager;
use crate::types::TableSchema;

pub fn load_all(pager: &mut Pager) -> DbResult<Vec<TableSchema>> {
    let tree = BTree {
        root: pager.header.catalog_root,
    };
    let mut out = Vec::new();
    let mut decode_err = None;
    tree.scan(
        pager,
        Bound::Unbounded,
        Bound::Unbounded,
        &mut |id, bytes| match TableSchema::decode(id, bytes) {
            Ok(schema) => {
                out.push(schema);
                Ok(true)
            }
            Err(e) => {
                decode_err = Some(e);
                Ok(false)
            }
        },
    )?;
    if let Some(e) = decode_err {
        return Err(e);
    }
    Ok(out)
}

/// Insert or update one table's schema. If the catalog tree's root splits,
/// the new root is recorded in the header (made durable at commit).
pub fn save(pager: &mut Pager, schema: &TableSchema) -> DbResult<()> {
    let mut tree = BTree {
        root: pager.header.catalog_root,
    };
    tree.insert(pager, schema.id, &schema.encode())?;
    if tree.root != pager.header.catalog_root {
        pager.header.catalog_root = tree.root;
        pager.header_dirty = true;
    }
    Ok(())
}

pub fn remove(pager: &mut Pager, table_id: i64) -> DbResult<()> {
    let mut tree = BTree {
        root: pager.header.catalog_root,
    };
    tree.delete(pager, table_id)?;
    if tree.root != pager.header.catalog_root {
        pager.header.catalog_root = tree.root;
        pager.header_dirty = true;
    }
    Ok(())
}
