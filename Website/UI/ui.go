// Package ui holds the website's pages, static files, installer template and
// the recorded CLI session the home page illustrates. They are embedded into
// the server binary, so a release is one immutable file.
package ui

import "embed"

//go:embed Pages Static Install Data
var Files embed.FS
