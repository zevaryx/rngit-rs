#!/bin/sh
# Stands in for $EDITOR: writes $EDIT_TEXT into the file the caller opened.
printf "%s" "$EDIT_TEXT" > "$1"
