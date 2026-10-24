# Licensing scope

Required Notice: Copyright (c) 2026 leeguooooo (https://github.com/leeguooooo/iphone-use)

The project's original code and documentation in this revision are offered under
[PolyForm Shield 1.0.0](LICENSE), except where a separate license or third-party
notice applies. The standard license text is reproduced without modification.
This is source-available software, not an OSI-approved open-source license.
The license permits non-competing uses subject to its terms; it does not prohibit
all commercial use. Its competition restriction includes free competing products.
For uses outside that grant, request separate permission from the copyright holder
through the [project repository](https://github.com/leeguooooo/iphone-use).

## License metadata

PolyForm Shield 1.0.0 is not in the [SPDX License List](https://github.com/spdx/license-list-data/blob/main/json/licenses.json) (checked October 24, 2026). The listed PolyForm Noncommercial and Small Business licenses have different terms and must not be substituted as identifiers for Shield.

Rust manifests therefore use `license-file` to reference this repository's LICENSE. The private root package metadata uses `SEE LICENSE IN LICENSE`. These file references do not change the license grant.

## Third-party material

Third-party material retains its existing licenses and copyright notices. This
change does not replace those grants or claim exclusive ownership of that material.
In particular:

- Portions of the device runner are adapted from Callstack's agent-device under
  MIT. Preserve the attribution headers and the complete notice in
  [runner/README.md](runner/README.md#attribution).
- Portions of the Jev integration are adapted from Browser Use's jev-ultrafast
  under MIT. Its complete notice remains in
  [crates/mcp/src/jev.rs](crates/mcp/src/jev.rs).
- The idevice dependency used by legacy-launch retains its upstream MIT license.
  Other dependencies retain their respective upstream licenses.

When distributing binaries that incorporate third-party material, include the
applicable third-party notices as well as the project license and Required Notice.

## Previously published MIT versions

This change applies to the licensing offered for this revision and future
revisions. It does not revoke permissions already granted for copies previously
published under MIT. This section records the previous project license; it does
not offer newly added project code under MIT. Existing third-party MIT material
remains subject to its own grants described above.

```text
MIT License

Copyright (c) 2026 leeguooooo

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
