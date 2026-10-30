(almost) completely ai slop, didnt look at the code too much, if this gets more than like 10 stars i might
rewrite it more cleanly, the code is not the best and still has some common ai slop issues left but at least
i tried to clean up a bit

you can also sort of tell which parts the llms wrote because i almost never capitalize things
anyways please read the entire part bellow:
# slopfile

coding agent/harness/clm however you want to call it, where the entire context window
with all thw tool calls and shit is serialized into a god awful markdown format 
on every api turn.
this allows the llm to edit its own context window
intially i was making this so that i could edit the context window myself to get 
llms unstuck from shitty trajectories and make it do things it would normally not
do by coming up with a moral justification in its reasoning,
but turns out that letting the llm edit its own context window,
something i explicitly prevented before, was a pretty good idea

i dont really know how it works, the actual implementation is a result
of yelling at whatever was free on opencode at the time (dsv4 flash, then minimax m3.1)
most of the code looks reasonable tho aside from the billion tests

also a big note is that this is not intended to run outside of docker because there are no safety features at all, expect the llm to break everything

all of the functionality related to llm self context editing is inspired by https://youtube.com/watch?v=aYGN4yDYDvk
the claims made in the video might not work for this thing though, but i belive that its flexible enough to just be solved by prompting enough. see prompt.rs for the default system prompt. 
the file format that i have is a little stupid because i didnt build it from ground up to be llm readable, only human readable, but by the time i got to actually having the model edit its own context the format became unreadable shit anyways so ¯\\_(ツ)_/¯ 
because the file format is rather ass, there is a python tool for parsing it + skill included in /shared/skills by default. its probably possible to get the model to edit it if you ask idk tho its pretty good as is i think
also if you dont tell the model to cut down its own context its pretty unlikely for it to just remember randomly to do that on its own
the context editing is indended to be done via the included skill/python tool, which may be out of sync from the rust code, 
that was an intentional decision so that people could implement their own parsers (by that i mean tell the model to figure it out on its own) 
so that there is a bit more freedom to that i guess
the default implementation is aint the best honestly but i dont have enough tokens to make it good lol
# running

look at how `start.sh` does it
there is also `./start.sh --no-llama` to only start the daemon
it expects you to be in the docker group
the daemon expects env variables something like this:
```
SLOPFILE_CONFIG=shared/slopfile.toml
SLOPFILE_API_BASE=http://127.0.0.1:8080
SLOPFILE_WORKDIR=/path/to/a/codebase
SLOPFILE_TRANSCRIPTS=./transcripts
```
but like you really should be using the docker script, this thing got no security at all so yea

the daemon is innert until you send a request to it with the second binary `sf`
`sf send` arms a session.
the intended vision here is to have a terminal open to just send these next to your editor,
or if you use the nvim plugin that start.sh includes, it gives you :Send :Stop etc as commands

```bash
sf init <session>                # create a session file
sf send <session>                # start generation
sf stop <session>                # let the turn finish, then idle
sf abort <session>               # stops immediately
sf status <session>              # one line of daemon state
sf question <session> <answers…> # answer a question from the question tool that the llm has by default
sf tokens <session>              # token coumt
```

`sf` reaches the daemon over `--addr`, `SF_ADDR`, or the default
`127.0.0.1:8787`.

other stuff

```bash
./start.sh                  # daemon + llama-server + open file in nvim with the plugin
./start.sh --no-llama       # dont start llama-server docker image
docker/run.sh logs slopfile # logs
docker/run.sh down          # stop 
```
## OpenRouter

In `slopfile.toml`:

```toml
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
reasoning_style = "object"
headers = { Authorization = "Bearer ${OPENROUTER_API_KEY}" }
```
`reasoning_style = "object"` is needed because the name of the reasoning effort key is different with openai/llama ¯\\_(ツ)_/¯ 
the ${} thing is for env variables, but because its in docker, you have to mirror them
in the docker compose thingy too

Then in the session's fence, the `` ```toml `` block at the top of the transcript:

````markdown
```toml
provider = "openrouter"
model = "dots-studio/dots-3-note-preview:free"
thinking = "high"
tools = ["read", "write", "edit", "bash", "grep", "find", "ls", "question", "subagent"]
```
````

```yaml
OPENROUTER_API_KEY: ${OPENROUTER_API_KEY:-}
```
## Any OpenAI-compatible endpoint

Same shape, any provider with `POST /chat/completions`:

```toml
[providers.openai]
base_url = "https://api.openai.com/v1"
headers = { Authorization = "Bearer ${OPENAI_API_KEY}" }

[providers.mylocal]
base_url = "http://192.168.1.50:8080"
```

A provider that is a llama-server wants `llama = true`, which turns back
on the llama.cpp routes (`/models/load`, `/abort`, `/tokenize`) that hosted
endpoints skip.

# misc
- there is a known issue of the clanker running out of max token output sometimes, not sure if thats something i can fix
- if a model edits its own context window so that its some custom tag like "notes" at the end, harness exits out of the loop (might be fixed now?)
- this does not implement any image/video reading capabilities yet, maybe one day ill get deepseek to do it with something like @/path/to/image, but that messes with the entire context window as a fole thing, but i also dont want to have it as a base64 string or anything like that. the only real way of doing this entirely in file that i see would be to store the relatively low res latent state of the vision encoder, but i dont think there is an api for that
- the file format should have been json instead of the markdown comment bullshit i went with, maybe one day ill get deepseek to rewrite that too. a moddified version of json would go hard. rn its some weird pseudo json but instead of {} "" [] i have the dumb comment marker
- ./shared/skills/thermo-nuclear-code-quality-review.md is taken from https://github.com/cursor/plugins/blob/main/cursor-team-kit/skills/thermo-nuclear-code-quality-review/SKILL.md with slight moddifications
tools are not a part of the system prompt, see `pub fn definitions()` in tools.rs, it would get ugly if they were in the prompt too, and they are all defined in rust anyways so like why would you have custom ones at all
- also, need to get MCP working one day too
