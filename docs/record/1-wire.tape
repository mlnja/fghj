# Clip 1: an empty machine. Make a folder, wire the entry repo, look at its config.
#   vhs 1-wire.tape   -> 1-wire.mp4
# It runs ./reset.sh first, so every take starts from nothing.
Output 1-wire.mp4

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

# Everything between Hide and Show runs but is not recorded.
# exec zsh -f: a clean shell, so your own prompt theme stays out of the shot.
Hide
Type "exec zsh -f"
Enter
Sleep 500ms
Type "setopt interactivecomments && PS1='%F{green}$%f '"
Enter
Type "./reset.sh"
Enter
Wait@60s
Type "cd ~ && clear"
Enter
Sleep 500ms
Show

Sleep 400ms
Type "mkdir ~/files && cd ~/files"
Sleep 300ms
Enter
Sleep 600ms

Type "fghj wire https://github.com/fghj-examples/files-web.git"
Sleep 300ms
Enter
Wait@60s
Sleep 1.5s

# The whole file, minus its comments, so it fits on one screen.
Type `grep -v '#' files-web/.fghj.yaml | cat -s`
Sleep 300ms
Enter
Sleep 5s
