# Turns `cargo make <task> [--debug|--release]` into the two things every
# build task needs. Dot-source it with the task's own arguments:
#
#     . ./scripts/build-flags.ps1 @args
#
# and it defines, in the caller's scope:
#
#     $flags      - the arguments to forward to cargo
#     $profileDir - the target/ subdirectory the artifacts land in
#
# `--debug` is documented (README) and the rest of the flow accepts it, but
# cargo itself has NO --debug flag (debug is its default), so forwarding it
# verbatim made the documented command fail outright (issue #5). Dropping it
# here is what makes both spellings work.
#
# One file because four tasks had the `$flags` line copied and three
# different spellings of the profile mapping existed side by side. ASCII
# only, like the other scripts here: Windows PowerShell 5.1 reads a BOM-less
# UTF-8 script as CP932.

$flags = @($args | Where-Object { $_ -and $_ -ne '--debug' })
$profileDir = if ($flags -contains '--release') { 'release' } else { 'debug' }
