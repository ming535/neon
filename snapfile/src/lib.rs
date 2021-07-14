//! A file format for storage a snapshot of pages.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![warn(clippy::cast_possible_truncation)]

mod page;
mod squash;
mod versioned;

#[doc(inline)]
pub use page::Page;

#[doc(inline)]
pub use squash::squash;

use anyhow::{bail, Context, Result};
use aversion::group::{DataSink, DataSourceExt};
use aversion::util::cbor::CborData;
use bookfile::{Book, BookWriter, ChapterWriter};
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::ops::AddAssign;
use std::path::{Path, PathBuf};
pub use versioned::{PageIndex, PageLocation, Predecessor, SnapFileMeta};
use zenith_utils::lsn::Lsn;

impl SnapFileMeta {
    pub fn new(previous: Option<SnapFileMeta>, timeline: [u8; 16], lsn: Lsn) -> Self {
        // Store the metadata of the predecessor snapshot, if there is one.
        let predecessor = previous.map(|prev| Predecessor {
            timeline: prev.timeline,
            lsn: prev.lsn,
        });

        SnapFileMeta {
            timeline,
            predecessor,
            lsn: lsn.into(),
        }
    }

    fn to_filename(&self) -> OsString {
        let timeline_string = hex::encode(self.timeline);
        let pred_lsn = match &self.predecessor {
            None => 0,
            Some(pred) => pred.lsn,
        };
        format!("{}_{:x}_{:x}.zdb", timeline_string, pred_lsn, self.lsn).into()
    }
}

impl PageIndex {
    /// Retrieve the page offset from the index.
    ///
    /// If the page is not in the index, returns `None`.
    fn get_page_location(&self, page_num: u64) -> Option<PageLocation> {
        self.map.get(&page_num).copied()
    }

    fn page_count(&self) -> usize {
        self.map.len()
    }
}

impl PageLocation {
    fn to_offset(&self) -> u64 {
        // Counts in units of one page.
        self.0 * 8192
    }
}

impl AddAssign<u64> for PageLocation {
    fn add_assign(&mut self, rhs: u64) {
        self.0 += rhs;
    }
}

/// A read-only snapshot file.
pub struct SnapFile {
    book: Book<File>,
    page_index: PageIndex,
}

impl SnapFile {
    /// Open a new `SnapFile` for reading.
    ///
    /// This call will validate some of the file's format and read the file's
    /// metadata; it may return an error if the file format is invalid.
    pub fn new(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("snapfile {}", path.to_string_lossy()))?;
        let book = Book::new(file)?;
        if book.magic() != versioned::SNAPFILE_MAGIC {
            bail!("bad magic number");
        }

        // Read the page index into memory.
        let chapter_reader = book
            .chapter_reader(versioned::CHAPTER_PAGE_INDEX)
            .context("snapfile missing index chapter")?;
        let mut source = CborData::new(chapter_reader);
        let page_index: PageIndex = source.expect_message()?;
        Ok(SnapFile { book, page_index })
    }

    /// Read the snapshot metadata.
    pub fn read_meta(&mut self) -> Result<SnapFileMeta> {
        let chapter_reader = self
            .book
            .chapter_reader(versioned::CHAPTER_SNAP_META)
            .context("snapfile missing meta")?;
        let mut source = CborData::new(chapter_reader);
        let meta: SnapFileMeta = source.expect_message()?;
        Ok(meta)
    }

    /// Return the number of pages stored in this snapshot.
    pub fn page_count(&self) -> usize {
        self.page_index.page_count()
    }

    /// Check if a page exists in this snapshot's index.
    ///
    /// Returns `true` if the given page is stored in this snapshot file,
    /// `false` if not.
    pub fn has_page(&self, page_num: u64) -> bool {
        self.page_index.get_page_location(page_num).is_some()
    }

    /// Read a page.
    ///
    /// If this returns Ok(None), that means that this file does not store
    /// the requested page.
    /// This should only fail (returning `Err`) if an IO error occurs.
    pub fn read_page(&self, page_num: u64) -> Result<Option<Page>> {
        match self.page_index.get_page_location(page_num) {
            None => Ok(None),
            Some(page_offset) => Ok(Some(self._read_page(page_offset)?)),
        }
    }

    /// Read page data from the file.
    ///
    /// This does the work for read_page and PageIter.
    fn _read_page(&self, page_location: PageLocation) -> Result<Page> {
        // Compute the true byte offset in the file.
        let page_offset = page_location.to_offset();
        let chapter_reader = self
            .book
            .chapter_reader(versioned::CHAPTER_PAGES)
            .context("snapfile missing pages chapter")?;

        let mut page_data = Page::default();
        let bytes_read = chapter_reader.read_at(page_data.as_mut(), page_offset)?;
        if bytes_read != 8192 {
            bail!("read truncated page");
        }
        Ok(page_data)
    }

    /// Iterate over pages.
    ///
    /// This will return an iterator over (usize, )
    pub fn all_pages(&self) -> PageIter {
        let inner = (&self.page_index.map).into_iter();
        PageIter {
            snapfile: self,
            inner,
        }
    }
}

/// An iterator over all pages in the snapshot file.
pub struct PageIter<'a> {
    snapfile: &'a SnapFile,
    inner: std::collections::btree_map::Iter<'a, u64, PageLocation>,
}

impl Iterator for PageIter<'_> {
    type Item = Result<(u64, Page)>;

    fn next(&mut self) -> Option<Self::Item> {
        let (page_num, page_offset) = self.inner.next()?;
        let result = self
            .snapfile
            ._read_page(*page_offset)
            .map(|page_data| (*page_num, page_data));
        Some(result)
    }
}

/// `SnapWriter` creates a new snapshot file.
///
/// A SnapWriter is created, has pages written into it, and is then closed.
pub struct SnapWriter {
    writer: ChapterWriter<File>,
    page_index: PageIndex,
    meta: SnapFileMeta,
    current_offset: PageLocation,
}

impl SnapWriter {
    /// Create a new `SnapWriter`.
    ///
    pub fn new(dir: &Path, meta: SnapFileMeta) -> Result<Self> {
        let mut path = PathBuf::from(dir);
        path.push(meta.to_filename());
        let file = File::create(path)?;
        let book = BookWriter::new(file, versioned::SNAPFILE_MAGIC)?;

        // Write a chapter for the snapshot metadata.
        let writer = book.new_chapter(versioned::CHAPTER_SNAP_META);
        let mut sink = CborData::new(writer);
        sink.write_message(&meta)?;
        let book = sink.into_inner().close()?;

        // Open a new chapter for raw page data.
        let writer = book.new_chapter(versioned::CHAPTER_PAGES);
        Ok(SnapWriter {
            writer,
            page_index: PageIndex::default(),
            meta,
            current_offset: PageLocation::default(),
        })
    }

    /// Write a page into the snap file.
    pub fn write_page<P>(&mut self, page_num: u64, page_data: P) -> Result<()>
    where
        P: Into<Page>,
    {
        let page_data: Page = page_data.into();
        self.writer.write_all(page_data.as_ref())?;
        let prev = self.page_index.map.insert(page_num, self.current_offset);
        if prev.is_some() {
            panic!("duplicate index for page {}", page_num);
        }
        self.current_offset += 1;
        Ok(())
    }

    /// Finish writing pages.
    ///
    /// This consumes the PagesWriter and completes the snapshot.
    //
    pub fn finish(self) -> Result<SnapFileMeta> {
        let book = self.writer.close()?;

        // Write out a page index and close the book. This will write out any
        // necessary file metadata.
        // FIXME: these 3 lines could be combined into a single function
        // that means "serialize this data structure with this format into this chapter".
        let writer = book.new_chapter(versioned::CHAPTER_PAGE_INDEX);
        let mut sink = CborData::new(writer);
        sink.write_message(&self.page_index)?;

        // Close the chapter, then close the book.
        sink.into_inner().close()?.close()?;

        // Return the snapshot metadata to the caller.
        Ok(self.meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const TEST_TIMELINE: [u8; 16] = [99u8; 16];

    #[test]
    fn snap_two_pages() {
        // When `dir` goes out of scope the directory will be unlinked.
        let dir = TempDir::new().unwrap();
        let snap_meta = {
            // Write out a new snapshot file with two pages.
            let meta = SnapFileMeta::new(None, TEST_TIMELINE, Lsn(1234));
            let mut snap = SnapWriter::new(dir.path(), meta).unwrap();
            // Write the pages out of order, because why not?
            let page99 = [99u8; 8192];
            snap.write_page(99, page99).unwrap();
            let page33 = [33u8; 8192];
            snap.write_page(33, page33).unwrap();
            snap.finish().unwrap()
        };

        assert_eq!(snap_meta.lsn, 1234);

        {
            // Read the snapshot file and verify the contents.
            let mut path = PathBuf::from(dir.path());
            path.push(snap_meta.to_filename());
            let mut snap = SnapFile::new(&path).unwrap();

            assert_eq!(snap.page_count(), 2);
            assert!(!snap.has_page(0));
            assert!(snap.has_page(33));
            assert!(!snap.has_page(98));
            assert!(snap.has_page(99));
            assert!(snap.read_page(0).unwrap().is_none());
            let page = snap.read_page(33).unwrap().unwrap();
            assert_eq!(*page.0, [33u8; 8192]);
            let page = snap.read_page(99).unwrap().unwrap();
            assert_eq!(*page.0, [99u8; 8192]);

            // Make sure the deserialized metadata matches what we think we wrote.
            let meta2 = snap.read_meta().unwrap();
            assert_eq!(snap_meta, meta2);
        }
    }

    #[test]
    fn snap_zero_pages() {
        // When `dir` goes out of scope the directory will be unlinked.
        let dir = TempDir::new().unwrap();
        let snap_meta = {
            // Write out a new snapshot file with no pages.
            let meta = SnapFileMeta::new(None, TEST_TIMELINE, Lsn(1234));
            let snap = SnapWriter::new(dir.path(), meta).unwrap();
            snap.finish().unwrap()
        };

        {
            // Read the snapshot file.
            let mut path = PathBuf::from(dir.path());
            path.push(snap_meta.to_filename());
            let snap = SnapFile::new(&path).unwrap();
            assert_eq!(snap.page_index.page_count(), 0);
            assert!(!snap.has_page(0));
            assert!(!snap.has_page(99));
            assert!(snap.read_page(0).unwrap().is_none());
            assert!(snap.read_page(99).unwrap().is_none());
        }
    }
}
