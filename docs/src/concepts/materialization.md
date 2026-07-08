# Materialization

In Okayeg, the doc is the source of truth, which interfaces (such as eg)
project into a local working copy.

## Different projection methods

There are different ways that a doc can appear to the user:

- `eg ls`, `eg cat` can show the contents of a doc without necessarily
  materializing it to disk.
- Real file materialization
  - Default method
  - Can also use `eg export` to materialize docs
- Virtual file using FUSE (`eg mount`)

Note that comments are not handled in any projection method, requiring a
different interface to view and edit them.

### Real file materialization

Okayeg uses a Loro tree doc type to structure files and directories, where each
node in the tree is a file or directory. eg materializes this doc so that the
directory reflects the tree structure with the same file names and content.

### Virtual file materialization

eg can also materialize a doc as a virtual file system using FUSE. This allows
the user to interact with the doc using standard filesystem operations without
actually writing the files to disk. In many cases, this may be better since
there is no drift between the doc and the materialized files.

## Editing files live

The editor can also read and write to the Loro doc directly while delaying live
materialization. Any changes that occur during editing reflect in the doc, but
these changes are not yet saved to disk.

However, the user can still trigger a file materialization through their
editor; what editor materialization mode does is that it only pauses the normal
materialization process for opened files, only materializing them when the user
saves the file, or whenever the file is closed.
