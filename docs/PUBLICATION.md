# Keeping private data out of source control

Use synthetic dialogue or attributed public speech corpora for committed fixtures. Never commit captured conversations, speaker embeddings, database copies, authentication tokens, or raw benchmark outputs derived from personal recordings. The model-refresh command writes complete transcripts; its dated output files are local-only and ignored.

The historical research notebook and captured benchmark results were removed during publication preparation. The public research overview retains aggregate measurements. Earlier published packages require a separate review; cleaning Git does not change release attachments.

Use configurable paths for private local data. Research scripts use `/tmp/nx-recall-workspace` as a generic scratch location; point them at your own local inputs when running them.

Before publishing changes, inspect `git diff --cached`, scan credentials across Git history, and check that no generated outputs have been force-added. `.gitignore` does not remove files already committed. If private material is discovered in history, keep the repository private until every affected branch, tag, and release has been addressed.
