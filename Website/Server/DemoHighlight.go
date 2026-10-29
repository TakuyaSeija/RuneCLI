package main

import (
	"html"
	"html/template"
	"strings"
)

// Only fixed emphasis tags are generated. Both their content and all remaining
// source are escaped first; markdown can never create executable HTML or URLs.
func demoInline(source string) template.HTML {
	var out strings.Builder
	for len(source) > 0 {
		marker, tag := "", ""
		if strings.HasPrefix(source, "**") {
			marker, tag = "**", "strong"
		} else if strings.HasPrefix(source, "`") {
			marker, tag = "`", "code"
		}
		if marker != "" {
			if end := strings.Index(source[len(marker):], marker); end >= 0 {
				inner := source[len(marker) : len(marker)+end]
				out.WriteString("<" + tag + ">")
				if tag == "strong" {
					// Code spans inside bold stay code, as the CLI draws them.
					out.WriteString(string(demoInline(inner)))
				} else {
					out.WriteString(html.EscapeString(inner))
				}
				out.WriteString("</" + tag + ">")
				source = source[len(marker)*2+end:]
				continue
			}
		}
		at := 1
		for at < len(source) && source[at] != '`' && source[at] != '*' {
			at++
		}
		out.WriteString(html.EscapeString(source[:at]))
		source = source[at:]
	}
	return template.HTML(out.String())
}
