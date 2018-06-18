use btree::{self, NodeStore, SeekBias};
use id;
use smallvec::SmallVec;
use std::cmp::{self, Ordering};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::ops::{Add, AddAssign};
use std::path::{Path, PathBuf};
use std::sync::Arc;

trait Store {
    type ReadError: fmt::Debug;
    type ItemStore: NodeStore<Item, ReadError = Self::ReadError>;
    type InodeToFileIdStore: NodeStore<InodeToFileId, ReadError = Self::ReadError>;

    fn item_store(&self) -> &Self::ItemStore;
    fn inode_to_file_id_store(&self) -> &Self::InodeToFileIdStore;
    fn gen_id(&self) -> id::Unique;
}

#[derive(Clone)]
struct Tree {
    items: btree::Tree<Item>,
    file_ids_by_inode: btree::Tree<InodeToFileId>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct Inode(u64);

#[derive(Clone, Debug, Eq, PartialEq)]
enum Item {
    Metadata {
        file_id: id::Unique,
        is_dir: bool,
        inode: Inode,
    },
    DirEntry {
        file_id: id::Unique,
        entry_id: id::Unique,
        child_id: id::Unique,
        name: Arc<OsString>,
        is_dir: bool,
        deletions: SmallVec<[id::Unique; 1]>,
        moves: SmallVec<[Move; 1]>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Move {
    file_id: id::Unique,
    entry_id: id::Unique,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InodeToFileId {
    inode: Inode,
    file_id: id::Unique,
}

#[derive(Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
struct Key {
    file_id: id::Unique,
    kind: KeyKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum KeyKind {
    Metadata,
    DirEntry { is_dir: bool, name: Arc<OsString> },
}

struct Metadata {
    inode: Inode,
    is_dir: bool,
}

struct Builder {
    tree: Tree,
    stack: Vec<id::Unique>,
    cursor: Cursor,
    item_changes: Vec<ItemChange>,
    new_mappings: Vec<InodeToFileId>,
    insertions_by_inode: HashMap<Inode, id::Unique>,
}

enum ItemChange {
    InsertDirEntry {
        file_id: id::Unique,
        name: OsString,
        is_dir: bool,
        child_id: id::Unique,
        child_inode: Inode,
    },
    RemoveDirEntry {
        entry: Item,
        inode: Inode,
    },
}

struct Cursor {
    path: PathBuf,
    stack: Vec<btree::Cursor<Item>>,
}

impl Metadata {
    fn dir<I: Into<Inode>>(inode: I) -> Self {
        Metadata {
            is_dir: true,
            inode: inode.into(),
        }
    }

    fn file<I: Into<Inode>>(inode: I) -> Self {
        Metadata {
            is_dir: false,
            inode: inode.into(),
        }
    }
}

impl Tree {
    pub fn new() -> Self {
        Self {
            items: btree::Tree::new(),
            file_ids_by_inode: btree::Tree::new(),
        }
    }

    pub fn cursor<S: Store>(&self, db: &S) -> Result<Cursor, S::ReadError> {
        Cursor::new(self, db)
    }

    pub fn id_for_path<I: Into<PathBuf>, S: Store>(
        &self,
        path: I,
        db: &S,
    ) -> Result<Option<id::Unique>, S::ReadError> {
        let path = path.into();
        let item_db = db.item_store();
        let mut parent_file_id = id::Unique::default();
        let mut cursor = self.items.cursor();

        for component in path.components() {
            cursor.seek(
                &Key {
                    file_id: parent_file_id,
                    kind: KeyKind::Metadata,
                },
                SeekBias::Right,
                item_db,
            )?;

            let component_name = Arc::new(OsString::from(component.as_os_str()));

            cursor.seek_forward(
                &Key {
                    file_id: parent_file_id,
                    kind: KeyKind::DirEntry {
                        is_dir: true,
                        name: component_name.clone(),
                    },
                },
                SeekBias::Left,
                item_db,
            )?;

            match cursor.item(item_db)? {
                Some(Item::DirEntry {
                    name: entry_name,
                    child_id,
                    ..
                }) => {
                    if component_name == entry_name {
                        parent_file_id = child_id;
                    } else {
                        return Ok(None);
                    }
                }
                _ => return Ok(None),
            }
        }

        Ok(Some(parent_file_id))
    }

    #[cfg(test)]
    fn paths<S: Store>(&self, store: &S) -> Vec<String> {
        let mut paths = Vec::new();
        let mut cursor = self.cursor(store).unwrap();
        loop {
            paths.push(cursor.path().to_string_lossy().into_owned());
            if !cursor.next(store).unwrap() {
                return paths;
            }
        }
    }
}

impl From<u64> for Inode {
    fn from(inode: u64) -> Self {
        Inode(inode)
    }
}

impl<'a> Add<&'a Self> for Inode {
    type Output = Inode;

    fn add(self, other: &Self) -> Self::Output {
        cmp::max(self, *other)
    }
}

impl<'a> AddAssign<&'a Self> for Inode {
    fn add_assign(&mut self, other: &Self) {
        *self = cmp::max(*self, *other);
    }
}

impl btree::Dimension for Inode {
    type Summary = Self;

    fn from_summary(summary: &Self) -> &Self {
        summary
    }
}

impl Item {
    fn key(&self) -> Key {
        match self {
            Item::Metadata { file_id, .. } => Key::metadata(*file_id),
            Item::DirEntry {
                file_id,
                is_dir,
                name,
                ..
            } => Key::dir_entry(*file_id, *is_dir, name.clone()),
        }
    }

    fn is_dir_metadata(&self) -> bool {
        match self {
            Item::Metadata { is_dir, .. } => *is_dir,
            _ => false,
        }
    }

    fn is_dir_entry(&self) -> bool {
        match self {
            Item::DirEntry { .. } => true,
            _ => false,
        }
    }

    fn is_deleted(&self) -> bool {
        match self {
            Item::DirEntry { deletions, .. } => !deletions.is_empty(),
            _ => false,
        }
    }

    fn deletions_mut(&mut self) -> &mut SmallVec<[id::Unique; 1]> {
        match self {
            Item::DirEntry { deletions, .. } => deletions,
            _ => panic!(),
        }
    }
}

impl btree::Item for Item {
    type Summary = Key;

    fn summarize(&self) -> Self::Summary {
        self.key()
    }
}

impl Key {
    fn metadata(file_id: id::Unique) -> Self {
        Key {
            file_id,
            kind: KeyKind::Metadata,
        }
    }

    fn dir_entry(file_id: id::Unique, is_dir: bool, name: Arc<OsString>) -> Self {
        Key {
            file_id,
            kind: KeyKind::DirEntry { is_dir, name },
        }
    }
}

impl Default for Key {
    fn default() -> Self {
        Key::metadata(id::Unique::default())
    }
}

impl btree::Dimension for Key {
    type Summary = Self;

    fn from_summary(summary: &Self::Summary) -> &Self {
        summary
    }
}

impl<'a> AddAssign<&'a Self> for Key {
    fn add_assign(&mut self, other: &Self) {
        if *self < *other {
            *self = other.clone();
        }
    }
}

impl<'a> Add<&'a Self> for Key {
    type Output = Self;

    fn add(self, other: &Self) -> Self {
        if self < *other {
            other.clone()
        } else {
            self
        }
    }
}

impl PartialOrd for KeyKind {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KeyKind {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (KeyKind::Metadata, KeyKind::Metadata) => Ordering::Equal,
            (KeyKind::Metadata, KeyKind::DirEntry { .. }) => Ordering::Less,
            (KeyKind::DirEntry { .. }, KeyKind::Metadata) => Ordering::Greater,
            (
                KeyKind::DirEntry { is_dir, name },
                KeyKind::DirEntry {
                    is_dir: other_is_dir,
                    name: other_name,
                },
            ) => is_dir
                .cmp(other_is_dir)
                .reverse()
                .then_with(|| name.cmp(other_name)),
        }
    }
}

impl btree::Item for InodeToFileId {
    type Summary = Inode;

    fn summarize(&self) -> Inode {
        self.inode
    }
}

impl Builder {
    fn new<S: Store>(tree: Tree, db: &S) -> Result<Self, S::ReadError> {
        let cursor = tree.cursor(db)?;
        Ok(Self {
            tree,
            cursor,
            stack: Vec::new(),
            item_changes: Vec::new(),
            new_mappings: Vec::new(),
            insertions_by_inode: HashMap::new(),
        })
    }

    fn depth(&self) -> usize {
        self.stack.len()
    }

    fn push<N, S>(
        &mut self,
        name: N,
        metadata: Metadata,
        depth: usize,
        db: &S,
    ) -> Result<(), S::ReadError>
    where
        N: Into<OsString>,
        S: Store,
    {
        let name = name.into();
        let mut perform_insert = false;
        let mut file_id_to_push = None;

        while self.cursor.depth() > depth
            || self.cursor.depth() == depth
                && self.cursor.cmp_with_entry(name.as_os_str(), &metadata, db)? == Ordering::Less
        {
            self.item_changes.push(ItemChange::RemoveDirEntry {
                entry: self.cursor.dir_entry(db)?.unwrap(),
                inode: self.cursor.inode(db)?.unwrap(),
            });
            self.cursor.next(db)?;
        }
        self.stack.truncate(depth - 1);

        match depth.cmp(&self.cursor.depth()) {
            Ordering::Less => unreachable!(),
            Ordering::Equal => match self.cursor.cmp_with_entry(name.as_os_str(), &metadata, db)? {
                Ordering::Less => unreachable!(),
                Ordering::Equal => {
                    file_id_to_push = self.cursor.file_id(db)?;
                    self.cursor.next(db)?;
                }
                Ordering::Greater => perform_insert = true,
            },
            Ordering::Greater => perform_insert = true,
        };

        if perform_insert {
            let parent_id = self.stack.last().cloned().unwrap_or(id::Unique::default());
            let child_id = db.gen_id();
            self.item_changes.push(ItemChange::InsertDirEntry {
                file_id: parent_id,
                is_dir: metadata.is_dir,
                child_id,
                child_inode: metadata.inode,
                name,
            });
            file_id_to_push = Some(child_id);
        }

        self.stack.push(file_id_to_push.unwrap());

        Ok(())
    }

    fn tree<S: Store>(mut self, db: &S) -> Result<Tree, S::ReadError> {
        let item_db = db.item_store();

        while let Some(entry) = self.cursor.dir_entry(db)? {
            self.item_changes.push(ItemChange::RemoveDirEntry {
                entry,
                inode: self.cursor.inode(db)?.unwrap(),
            });
            self.cursor.next(db)?;
        }

        let mut new_items = Vec::new();
        for change in self.item_changes {
            match change {
                ItemChange::InsertDirEntry {
                    file_id: parent_dir_id,
                    name,
                    is_dir,
                    child_id,
                    child_inode,
                } => {
                    new_items.push(Item::Metadata {
                        file_id: child_id,
                        is_dir,
                        inode: child_inode,
                    });
                    new_items.push(Item::DirEntry {
                        file_id: parent_dir_id,
                        entry_id: db.gen_id(),
                        child_id,
                        name: Arc::new(name),
                        is_dir,
                        deletions: SmallVec::new(),
                        moves: SmallVec::new(),
                    });
                }
                ItemChange::RemoveDirEntry { mut entry, .. } => {
                    entry.deletions_mut().push(db.gen_id());
                    new_items.push(entry);
                }
            }
        }

        new_items.sort_unstable_by_key(|item| item.key());
        let mut old_items_cursor = self.tree.items.cursor();
        let mut new_tree = Tree::new();
        for item in new_items {
            new_tree.items.push_tree(
                old_items_cursor.slice(&item.key(), SeekBias::Left, item_db)?,
                item_db,
            )?;
            if item.is_deleted() {
                old_items_cursor.next(item_db)?;
            }
            new_tree.items.push(item, item_db)?;
        }
        new_tree
            .items
            .push_tree(old_items_cursor.suffix::<Key, _>(item_db)?, item_db)?;
        Ok(new_tree)
    }

    // fn find_or_create_file_id<S>(
    //     &mut self,
    //     metadata: &Metadata,
    //     db: &S,
    // ) -> Result<id::Unique, S::ReadError>
    // where
    //     S: Store,
    // {
    //     let inode_db = db.inode_to_file_id_store();
    //     let mut cursor = self.tree.file_ids_by_inode.cursor();
    //     cursor.seek(&metadata.inode, SeekBias::Left, inode_db)?;
    //     let mapping = cursor.item(inode_db)?;
    //     if mapping
    //         .as_ref()
    //         .map_or(false, |mapping| metadata.inode == mapping.inode)
    //     {
    //         Ok(mapping.unwrap().file_id)
    //     } else {
    //         let file_id = db.gen_id();
    //         self.new_mappings.push(InodeToFileId {
    //             inode: metadata.inode,
    //             file_id,
    //         });
    //         self.item_changes.push(ItemChange::Insert(Item::Metadata {
    //             file_id,
    //             is_dir: metadata.is_dir,
    //             inode: metadata.inode,
    //         }));
    //         Ok(file_id)
    //     }
    // }
}

impl Cursor {
    pub fn new<S>(tree: &Tree, db: &S) -> Result<Self, S::ReadError>
    where
        S: Store,
    {
        let item_db = db.item_store();
        let mut root_cursor = tree.items.cursor();
        root_cursor.seek(&Key::default(), SeekBias::Left, item_db)?;
        if let Some(item) = root_cursor.item(item_db)? {
            let mut cursor = Self {
                path: PathBuf::new(),
                stack: vec![root_cursor],
            };
            cursor.follow_entry(db)?;
            Ok(cursor)
        } else {
            Ok(Self {
                path: PathBuf::new(),
                stack: vec![],
            })
        }
    }

    pub fn cmp_with_entry<S: Store>(
        &self,
        other_name: &OsStr,
        other_metadata: &Metadata,
        db: &S,
    ) -> Result<Ordering, S::ReadError> {
        if let Some(self_metadata) = self.metadata(db)? {
            let ordering = other_metadata.is_dir.cmp(&self_metadata.is_dir);
            if ordering == Ordering::Equal {
                Ok(self.name(db)?.unwrap().as_os_str().cmp(other_name))
            } else {
                Ok(ordering)
            }
        } else {
            Ok(Ordering::Greater)
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata<S: Store>(&self, db: &S) -> Result<Option<Metadata>, S::ReadError> {
        if let Some(cursor) = self.stack.last() {
            match cursor.item(db.item_store())?.unwrap() {
                Item::Metadata { is_dir, inode, .. } => Ok(Some(Metadata { is_dir, inode })),
                _ => unreachable!(),
            }
        } else {
            Ok(None)
        }
    }

    pub fn dir_entry<S: Store>(&self, db: &S) -> Result<Option<Item>, S::ReadError> {
        if self.stack.len() > 1 {
            let cursor = &self.stack[self.stack.len() - 2];
            cursor.item(db.item_store())
        } else {
            Ok(None)
        }
    }

    pub fn file_id<S: Store>(&self, db: &S) -> Result<Option<id::Unique>, S::ReadError> {
        if let Some(cursor) = self.stack.last() {
            match cursor.item(db.item_store())?.unwrap() {
                Item::Metadata { file_id, .. } => Ok(Some(file_id)),
                _ => unreachable!(),
            }
        } else {
            Ok(None)
        }
    }

    pub fn inode<S: Store>(&self, db: &S) -> Result<Option<Inode>, S::ReadError> {
        if let Some(cursor) = self.stack.last() {
            match cursor.item(db.item_store())?.unwrap() {
                Item::Metadata { inode, .. } => Ok(Some(inode)),
                _ => unreachable!(),
            }
        } else {
            Ok(None)
        }
    }

    pub fn name<S: Store>(&self, db: &S) -> Result<Option<Arc<OsString>>, S::ReadError> {
        if self.stack.len() > 1 {
            let cursor = &self.stack[self.stack.len() - 2];
            match cursor.item(db.item_store())?.unwrap() {
                Item::DirEntry { name, .. } => Ok(Some(name.clone())),
                _ => unreachable!(),
            }
        } else {
            Ok(None)
        }
    }

    pub fn depth(&self) -> usize {
        self.stack.len().saturating_sub(1)
    }

    pub fn next<S: Store>(&mut self, db: &S) -> Result<bool, S::ReadError> {
        let item_db = db.item_store();
        while !self.stack.is_empty() {
            let found_entry = loop {
                let mut cursor = self.stack.last_mut().unwrap();
                let cur_item = cursor.item(item_db)?.unwrap();
                if cur_item.is_dir_entry() || cur_item.is_dir_metadata() {
                    cursor.next(item_db)?;
                    let next_item = cursor.item(item_db)?;
                    if next_item.as_ref().map_or(false, |i| i.is_dir_entry()) {
                        if next_item.unwrap().is_deleted() {
                            continue;
                        } else {
                            break true;
                        }
                    } else {
                        break false;
                    }
                } else {
                    break false;
                }
            };

            if found_entry {
                self.follow_entry(db)?;
                return Ok(true);
            } else {
                self.path.pop();
                self.stack.pop();
            }
        }

        Ok(false)
    }

    pub fn next_sibling<S: Store>(&mut self, db: &S) -> Result<bool, S::ReadError> {
        if self.stack.is_empty() {
            Ok(false)
        } else {
            let prev_depth = self.depth();
            self.stack.pop();
            self.path.pop();
            self.next(db)?;
            Ok(self.depth() == prev_depth)
        }
    }

    fn follow_entry<S: Store>(&mut self, db: &S) -> Result<(), S::ReadError> {
        let item_db = db.item_store();
        let mut child_cursor;
        {
            let entry_cursor = self.stack.last().unwrap();
            match entry_cursor.item(item_db)?.unwrap() {
                Item::DirEntry { child_id, name, .. } => {
                    child_cursor = entry_cursor.clone();
                    child_cursor.seek(&Key::metadata(child_id), SeekBias::Left, item_db)?;
                    self.path.push(name.as_ref());
                }
                _ => panic!(),
            }
        }
        self.stack.push(child_cursor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate rand;

    use self::rand::{Rng, SeedableRng, StdRng};
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn test_builder_basic() {
        let db = NullStore::new();
        let tree = Tree::new();
        let mut builder = Builder::new(tree, &db).unwrap();
        builder.push("a", Metadata::dir(1), 1, &db).unwrap();
        builder.push("b", Metadata::dir(2), 2, &db).unwrap();
        builder.push("c", Metadata::dir(3), 3, &db).unwrap();
        builder.push("d", Metadata::dir(4), 3, &db).unwrap();
        builder.push("e", Metadata::file(5), 3, &db).unwrap();
        builder.push("f", Metadata::dir(6), 1, &db).unwrap();
        let tree = builder.tree(&db).unwrap();
        assert_eq!(
            tree.paths(&db),
            ["a", "a/b", "a/b/c", "a/b/d", "a/b/e", "f"]
        );

        let mut builder = Builder::new(tree, &db).unwrap();
        builder.push("a", Metadata::dir(1), 1, &db).unwrap();
        builder.push("b", Metadata::dir(2), 2, &db).unwrap();
        builder.push("c", Metadata::dir(3), 3, &db).unwrap();
        builder.push("c2", Metadata::dir(7), 3, &db).unwrap();
        builder.push("d", Metadata::dir(4), 3, &db).unwrap();
        builder.push("e", Metadata::file(5), 3, &db).unwrap();
        builder.push("b2", Metadata::dir(8), 2, &db).unwrap();
        builder.push("g", Metadata::dir(9), 3, &db).unwrap();
        builder.push("f", Metadata::dir(6), 1, &db).unwrap();
        let tree = builder.tree(&db).unwrap();
        assert_eq!(
            tree.paths(&db),
            ["a", "a/b", "a/b/c", "a/b/c2", "a/b/d", "a/b/e", "a/b2", "a/b2/g", "f"]
        );

        let mut builder = Builder::new(tree, &db).unwrap();
        builder.push("a", Metadata::dir(1), 1, &db).unwrap();
        builder.push("b", Metadata::dir(2), 2, &db).unwrap();
        builder.push("d", Metadata::dir(4), 3, &db).unwrap();
        builder.push("e", Metadata::file(5), 3, &db).unwrap();
        builder.push("f", Metadata::dir(6), 1, &db).unwrap();
        let tree = builder.tree(&db).unwrap();
        assert_eq!(tree.paths(&db), ["a", "a/b", "a/b/d", "a/b/e", "f"]);
    }

    #[test]
    fn test_builder_random() {
        for seed in 0..100 {
            // println!("SEED: {}", seed);
            let mut rng = StdRng::from_seed(&[seed]);

            let mut store = NullStore::new();
            let store = &store;
            let mut next_inode = 0;

            let mut reference_tree = TestDir::gen(&mut rng, &mut next_inode, 0);
            let mut tree = Tree::new();
            let mut builder = Builder::new(tree.clone(), store).unwrap();
            reference_tree.build(&mut builder, 1, store);
            tree = builder.tree(store).unwrap();
            assert_eq!(tree.paths(store), reference_tree.paths());

            for _ in 0..5 {
                // eprintln!("=========================================");
                // eprintln!("existing paths {:#?}", tree.paths(store).len());
                // eprintln!("new tree paths {:#?}", reference_tree.paths().len());
                // eprintln!("=========================================");

                let mut moves = Vec::new();
                reference_tree.mutate(
                    &mut rng,
                    &mut PathBuf::new(),
                    &mut next_inode,
                    &mut moves,
                    0,
                );

                let mut builder = Builder::new(tree.clone(), store).unwrap();
                reference_tree.build(&mut builder, 1, store);
                let new_tree = builder.tree(store).unwrap();
                assert_eq!(new_tree.paths(store), reference_tree.paths());
                for m in moves {
                    if let Some(new_path) = m.new_path {
                        assert_eq!(
                            new_tree.id_for_path(&new_path, store),
                            tree.id_for_path(&m.old_path, store)
                        );
                    }
                }

                tree = new_tree;
            }
        }
    }

    #[test]
    fn test_key_ordering() {
        assert!(
            Key::dir_entry(id::Unique::default(), true, Arc::new("z".into()))
                < Key::dir_entry(id::Unique::default(), false, Arc::new("a".into()))
        );
    }

    const MAX_TEST_TREE_DEPTH: usize = 5;

    #[derive(Clone)]
    struct TestDir {
        name: OsString,
        inode: Inode,
        dir_entries: Vec<TestDir>,
    }

    struct Move {
        dir: TestDir,
        old_path: PathBuf,
        new_path: Option<PathBuf>,
    }

    impl TestDir {
        fn gen<T: Rng>(rng: &mut T, next_inode: &mut u64, depth: usize) -> Self {
            let new_inode = *next_inode;
            *next_inode += 1;

            let mut tree = Self {
                name: gen_name(rng),
                inode: Inode(new_inode),
                dir_entries: (0..rng.gen_range(0, MAX_TEST_TREE_DEPTH - depth + 1))
                    .map(|_| Self::gen(rng, next_inode, depth + 1))
                    .collect(),
            };
            tree.normalize_entries();
            tree
        }

        fn move_entry<T: Rng>(
            rng: &mut T,
            path: &mut PathBuf,
            moves: &mut Vec<Move>,
        ) -> Option<TestDir> {
            let name = gen_name(rng);
            path.push(&name);
            let mut removes = moves
                .iter_mut()
                .filter(|m| m.new_path.is_none())
                .collect::<Vec<_>>();
            if let Some(remove) = rng.choose_mut(&mut removes) {
                remove.new_path = Some(path.clone());
                let mut dir = remove.dir.clone();
                dir.name = name;
                return Some(dir);
            }
            path.pop();
            None
        }

        fn mutate<T: Rng>(
            &mut self,
            rng: &mut T,
            path: &mut PathBuf,
            next_inode: &mut u64,
            moves: &mut Vec<Move>,
            depth: usize,
        ) {
            path.push(&self.name);
            self.dir_entries.retain(|entry| {
                if rng.gen_weighted_bool(5) {
                    let mut entry_path = path.clone();
                    entry_path.push(&entry.name);
                    moves.push(Move {
                        dir: entry.clone(),
                        old_path: entry_path,
                        new_path: None,
                    });
                    false
                } else {
                    true
                }
            });
            for _ in 0..rng.gen_range(0, self.dir_entries.len() + 1) {
                rng.choose_mut(&mut self.dir_entries).unwrap().mutate(
                    rng,
                    path,
                    next_inode,
                    moves,
                    depth + 1,
                );
            }
            if depth < MAX_TEST_TREE_DEPTH {
                for _ in 0..rng.gen_range(0, 5) {
                    let moved_entry = if rng.gen_weighted_bool(4) {
                        Self::move_entry(rng, path, moves)
                    } else {
                        None
                    };
                    if let Some(moved_entry) = moved_entry {
                        self.dir_entries.push(moved_entry);
                    } else {
                        self.dir_entries.push(Self::gen(rng, next_inode, depth + 1));
                    }
                }
            }
            self.normalize_entries();
            path.pop();
        }

        fn normalize_entries(&mut self) {
            let mut existing_names = HashSet::new();
            self.dir_entries.sort_by(|a, b| a.name.cmp(&b.name));
            self.dir_entries.retain(|entry| {
                if existing_names.contains(&entry.name) {
                    false
                } else {
                    existing_names.insert(entry.name.clone());
                    true
                }
            });
        }

        fn paths(&self) -> Vec<String> {
            let mut cur_path = PathBuf::new();
            let mut paths = Vec::new();
            self.paths_recursive(&mut cur_path, &mut paths);
            paths
        }

        fn paths_recursive(&self, cur_path: &mut PathBuf, paths: &mut Vec<String>) {
            cur_path.push(self.name.clone());
            paths.push(cur_path.clone().to_string_lossy().into_owned());
            for dir in &self.dir_entries {
                dir.paths_recursive(cur_path, paths);
            }
            cur_path.pop();
        }

        fn build<S: Store>(&self, builder: &mut Builder, depth: usize, store: &S) {
            let name = self.name.clone();
            let metadata = Metadata::dir(self.inode.0);
            builder.push(name, metadata, depth, store).unwrap();
            for dir in &self.dir_entries {
                dir.build(builder, depth + 1, store);
            }
        }
    }

    fn gen_name<T: Rng>(rng: &mut T) -> OsString {
        let mut name = String::new();
        for _ in 0..rng.gen_range(1, 4) {
            name.push(rng.gen_range(b'a', b'z' + 1).into());
        }
        name.into()
    }

    #[derive(Debug)]
    struct NullStore {
        next_id: RefCell<id::Unique>,
    }

    impl NullStore {
        fn new() -> Self {
            Self {
                next_id: RefCell::new(id::Unique::random()),
            }
        }
    }

    impl Store for NullStore {
        type ReadError = ();
        type ItemStore = NullStore;
        type InodeToFileIdStore = NullStore;

        fn gen_id(&self) -> id::Unique {
            let next_id = self.next_id.borrow().clone();
            self.next_id.borrow_mut().inc();
            next_id
        }

        fn item_store(&self) -> &Self::ItemStore {
            self
        }

        fn inode_to_file_id_store(&self) -> &Self::InodeToFileIdStore {
            self
        }
    }

    impl btree::NodeStore<Item> for NullStore {
        type ReadError = ();

        fn get(&self, _id: btree::NodeId) -> Result<Arc<btree::Node<Item>>, Self::ReadError> {
            panic!("get should never be called on a null store")
        }
    }

    impl btree::NodeStore<InodeToFileId> for NullStore {
        type ReadError = ();

        fn get(
            &self,
            _id: btree::NodeId,
        ) -> Result<Arc<btree::Node<InodeToFileId>>, Self::ReadError> {
            panic!("get should never be called on a null store")
        }
    }
}
