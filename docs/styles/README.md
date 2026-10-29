# Dictation styles

Styles refine transcriptions locally using a downloadable text model. Choose a writing style for Personal, Work, Email, and Other, then assign apps to those categories. Handy selects the category when recording starts and uses it to guide the text model after speech recognition. Styles are optional and are disabled by default.

## Set up

1. Open **Styles** in the sidebar and enable **Use styles for dictation**.
2. In **Local style models**, download and select a model. Handy currently offers Qwen3 0.6B (about 429 MB) and Qwen2.5 0.5B Instruct (about 491 MB). The download size is not the same as runtime memory use. Downloads require an internet connection; the downloaded model runs locally.
3. Choose a default category and a writing style for each category.
4. In **App rules**, search for an installed app, select it, and choose its category. You can also select **Detect an app**, then switch to it within five seconds. Assign the detected app a category.
5. Optionally, add instructions under **Custom instructions**. Choose a category, edit its instructions, and save. Clear and save the field to remove the custom instructions. Unsaved drafts can be discarded or kept while switching between categories.

Available styles:

- **Formal:** standard capitalization and punctuation, with a professional tone.
- **Casual:** standard capitalization and lighter punctuation, with a conversational tone.
- **Very casual:** lowercase ordinary text with light punctuation. Technical identifiers and other text whose exact formatting matters can retain capitalization.

Category suggestions in the app list are informational: select a category to create a rule. An app with no rule uses the default category. Selecting **Use default category** removes its rule. Browser websites are not detected separately; different websites in one browser share its app rule. A single app can contain both personal and professional conversations, so select the category that best fits your use of that app.

## What happens during dictation

1. At recording start, Handy tries to identify the focused app. An explicit app rule determines the category; otherwise Handy uses the default category. It captures the category and style for that recording.
2. The selected speech model transcribes the audio. The Styles text model receives the transcript, category and style instructions, and any saved custom instructions for that category. It does not receive the audio.
3. Handy makes a narrow preprocessing pass for explicit adjacent clock-time corrections, then asks the local text model to refine the transcript. Handy checks the response for empty or invalid output, model control text, changed numbers and clock times, and changes to recognized technical tokens. Formal and very casual formatting are applied locally where appropriate.
4. If the model fails, times out, returns unusable output, or fails a preservation check, Handy uses the original transcript and reports that style processing was skipped. These checks do not detect every possible change in meaning; review important text.

The normal transcription shortcut uses Styles when enabled. The separate post-processing shortcut continues to use its configured post-processing prompt. Custom instructions are additional guidance; the model may not follow every instruction, and the selected style and output checks still apply. History records the selected style and category with the processing prompt; a retry uses the saved selection and current style settings.

## App detection

- **Linux:** installed-app discovery reads desktop entries from XDG application directories. Focus detection supports X11 (via `xprop`) and the Hyprland, Sway, and Niri Wayland compositor APIs. Other Wayland desktops may not expose the focused app to Handy; in that case the default category is used.
- **macOS:** discovery reads application bundle identifiers from common Applications folders. Focus detection uses the frontmost app's bundle identifier.
- **Windows:** discovery reads executable targets from Start Menu shortcuts. Focus detection uses the foreground process executable name.

Discovery may omit applications installed outside the searched locations, packaged apps without a suitable shortcut, and applications whose launcher and window identifiers differ. App rules store known launcher aliases, and manual detection can help identify an app. If detection is unavailable or an app is not assigned, the default category applies. The list does not read document contents or browser page titles.

## Privacy and troubleshooting

Style model files are stored in Handy's application data directory under `style-models`. Downloads are checked against expected size and SHA-256 before being installed. Local inference runs in an isolated worker process; the text is not sent to an external text-model provider by Styles. The worker has a 30-second timeout, and model loading and generation time depend on hardware and transcript length.

Development builds print the app identifier, category selection, model prompt, transcript, model response, and final styled output to the console. These diagnostics can contain private dictated text; avoid sharing logs without reviewing them. The detailed Styles console diagnostics are disabled in production builds.

If the output is unchanged, verify that Styles is enabled, the selected model is downloaded, and the correct app has an assigned category. Development logs show whether the category came from an app rule, the default for an unmatched app, or the default because foreground detection was unavailable. Model failures and rejected responses fall back to the original text. To run a development build when an installed Handy instance is already running, quit the existing instance first; Handy uses a single-instance mechanism.
