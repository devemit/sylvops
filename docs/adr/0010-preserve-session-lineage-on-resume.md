# Preserve Session lineage when resuming Conversations

Resume Conversation creates one successor Session instead of restarting or overwriting a terminal Session record. The daemon preserves a linear predecessor/successor lineage, inherits the Conversation identity into the successor, and lets clients present the transition as continuous while retaining truthful process history and preventing concurrent duplicate resumes. Opening an older predecessor follows the lineage to its latest Session; a live leaf is attached, while only an eligible terminal leaf may create another successor.
