# Clip 3: after "Download all" in the UI, the other repos are on disk.
#   vhs 3-ls.tape   -> 3-ls.mp4
Output 3-ls.mp4

Require fghj
Set Shell "zsh"
Set FontSize 20
Set Width 1200
Set Height 720
Set Padding 28
Set WindowBar Colorful
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
Set WaitPattern /\$\s*$/

Hide
Type "exec zsh -f"
Enter
Sleep 500ms
Type "cd ~/files && setopt interactivecomments && PS1='%F{green}$%f ' && clear"
Enter
Sleep 500ms
Show

Sleep 400ms
Type "ls"
Sleep 300ms
Enter
Sleep 2.5s
