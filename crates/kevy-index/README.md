# kevy-index

Declarative secondary indexes over prefix domains for kevy: `range` /
`unique` kinds over hash fields, maintained synchronously with writes
(derived-by-construction — verifiably zero drift), queried with cursor
pagination and two-index AND/OR composition. Pure logic crate: no I/O,
no runtime; the kevy server and embedded store wire it to their write
paths.
