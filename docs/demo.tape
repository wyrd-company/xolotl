# Records docs/assets/demo.gif. From the repository root, after `cargo build`:
#
#   vhs docs/demo.tape
#
# docs/demo-env.sh starts an OpenBao dev server in Docker and a debug xolotl.

Output docs/assets/demo.gif

Require bash
Require curl
Require docker
Require jq
Require openssl

Set Shell "bash"
Set FontSize 16
Set Width 1760
Set Height 680
Set Theme "Catppuccin Mocha"
Set Padding 24
Set Margin 12
Set MarginFill "#11111b"
Set BorderRadius 8
Set WindowBar "Colorful"
Set TypingSpeed 28ms

Hide
Type `source docs/demo-env.sh && cd "$XOLOTL_DEMO_DIR" && export PS1='demo> ' && clear` Enter
Sleep 15s
Show

Sleep 500ms
Type "cat agents.toml" Enter
Sleep 3s

Type "xolotl policies put agents.toml >/dev/null" Enter
Sleep 1s

Type "xolotl policies view agents" Enter
Sleep 5s

Type "xolotl keys issue --policy agents --name laptop --ttl 30d" Enter
Sleep 3s

Type "export XOLOTL_API_KEY=$(bao kv get -field=api_key secret/xolotl/keys/agents/laptop)" Enter
Sleep 2s

Type `curl -s $XOLOTL_URL/v1/models -H "Authorization: Bearer $XOLOTL_API_KEY" | jq -r '.data[].id'` Enter
Sleep 4s

Type "xolotl keys list" Enter
Sleep 5s

Hide
Type "xolotl_demo_down" Enter
Sleep 2s
