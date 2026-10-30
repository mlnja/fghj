# Clip 5: the same https URL answers on the host and inside a container.
# Run after the files-web/full flow is up:  vhs 5-same-url.tape  -> 5-same-url.mp4
Output 5-same-url.mp4

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
Require curl

Hide
Type "exec zsh -f"
Enter
Sleep 500ms
Type "cd ~/files && setopt interactivecomments && PS1='%F{green}$%f ' && clear"
Enter
Sleep 500ms
Show

Sleep 300ms
Type "URL=https://web.files-web.files.fghj.internal/api/health"
Enter
Sleep 500ms

Type "# from the host"
Enter
Type "curl -s $URL"
Sleep 300ms
Enter
Sleep 1.5s

Type "# from inside the api container, same URL, same certificate"
Enter
Type `fghj exec api.files-api -- node -e "fetch('$URL').then(r=>r.text()).then(console.log)"`
Sleep 300ms
Enter
Sleep 3.5s
