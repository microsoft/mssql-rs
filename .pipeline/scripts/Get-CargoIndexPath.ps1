# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Returns the Cargo sparse-index sub-path for a crate name.

.DESCRIPTION
  Shared helper dot-sourced by the sandbox feed scripts so the index-path layout
  (1/, 2/, 3/x/, ab/cd/) lives in one place. See
  https://doc.rust-lang.org/cargo/reference/registry-index.html#index-files
#>

function Get-CargoIndexPath {
    param([Parameter(Mandatory = $true)][string]$Name)
    $n = $Name.ToLower()
    switch ($n.Length) {
        1 { return "1/$n" }
        2 { return "2/$n" }
        3 { return "3/$($n[0])/$n" }
        default { return "$($n.Substring(0,2))/$($n.Substring(2,2))/$n" }
    }
}
