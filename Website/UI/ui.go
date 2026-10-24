// Package ui holds the website's pages, static files and installer template.
// They are embedded into the server binary, so a release is one immutable file.
package ui

import "embed"

//go:embed Pages Static Install
var Files embed.FS
