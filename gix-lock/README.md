Use lock-files in the way git does with auto-cleanup being the most notable feature.

* [x] writable lock files that can be committed to atomically replace the resource they lock
    * [x] optionally flush the new content to storage before, and its directory after, the replacement
* [x] read-only markers that lock a resource without the intend to overwrite it
* [x] auto-removal of the lockfiles and intermediate directories on drop or on signal
