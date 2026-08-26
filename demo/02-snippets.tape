Output "demo/02-snippets.mp4"
Set Shell "bash"
Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30
Set LoopOffset 20%
Hide
Type "lazybox --demo"
Enter
Wait+Screen@45s /rate limiter/
Show
Sleep 1500ms
Type "]]"
Sleep 700ms
Type "s"
Sleep 1800ms
Down@650ms 5
Sleep 1200ms
Screenshot "demo/02-snippets.png"
Sleep 1500ms
