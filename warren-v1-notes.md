Arrow keys on the color picker should only switch to another field at the edges--e.g. up-arrow in the color picker should switch to the 'title' field only if I'm in the top row. Currently, it switches field no matter where I am in the color picker, which is confusing, because left/right behave normally.

Warren v0's color bug is back; when I use <C-g> to open the text editor in Claude Code, I expect vis to render a white background and black text--I see a black background and blue text.

Claude's text-entry bar at the bottom is made of horizontal box-drawing chars; the sidebar is made of vertical box-drawing chars. If it's not infeasible, using `├` where they meet would be a nice aesthetic touch. Similarly, the sidebar divider should use the same color as the text-entry box's dividers.

The status-bar should have a separate color for the region that says 'NORMAL'/'EDIT'/'INSERT' (and maybe INSERT mode should be called CLAUDE mode?). Preference--CLAUDE mode is Anthropic Orange (you'd know the color code), NORMAL green, EDIT purple? Rest of the status-bar should be dark gray with white text.
- Related--EDIT mode is a nice touch, but it's odd to me that EDIT mode isn't also used for the new agent screen, when they're semantically very similar. 

Bring back the big color picker! Figure out how much screen real-estate is available, then figure out how big the boxes can be while keeping everything on-screen. It looked great in warren v0.
