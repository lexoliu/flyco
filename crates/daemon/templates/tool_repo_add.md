Ask for another GitHub repository to be cloned into this session's workspace.

The session's workspace holds each repository in its own directory beneath the workspace root — the one you are working in — and this tool requests one more. It is a *request*, not a clone: the user approves or denies it, because a repository they never picked is code they have not agreed to put on their machine. On approval the checkout lands in a directory named after the repository, and you are told when it is there.

Use it when the task needs code outside the repositories the session was opened with — a sibling library, a fixture repo, an upstream source to read. Do not use it for code you could fetch with a package manager or read with a web search: an added checkout is a full clone the session keeps, snapshots, and reports dirty.

`repo` is `owner/name`. `branch` is optional and defaults to the repository's default branch. `reason` is required and is what the user reads on the approval card — say which work needs the repository, not that you want it.
