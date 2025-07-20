# Simple Email Client Wrapper

I have several IMAP mailboxes.  Over the years I've used many different
email clients.  For the last few years I've used a combination of FOSS
tools to manage my email:

- `mbsync` to synchronize my IMAP servers with local maildir mailboxes
- `mu` to index and search emails in the mailboxes
- `neomutt` to read and send email

Coordinating these was a bit of a mess using shell scripts to run the
`mbsync/mu` updates in parallel and a `neomutt` macro to trigger them
on demand.  I hated waiting for the macro to finish everytime I wanted
to check for new email.  I found the built in background checks in 
`neomutt` to be unsatisfying and using cron to update the email in the
background had some problems, including the need to store my email
passwords in plain text rather than using the macos keychain.

Recently I've been learning rust so I decided to crack this egg with that
hammer.  The result is this little rust program that:

- Runs one thread per mailbox to periodically run a simple `mbsync + mu` script.
- Runs one thread to check for existence of a file that triggers an immediate (out of band) update when I'm impatient.
- Runs `neomutt` in the foreground.
- Cleanly terminates all background threads when `neomutt` exits.

This program solved several problems.

- The threads only run while `neomutt` runs so there are no updates happening when I'm not interested in email.
- Mailbox passwords can still live happily in the keychain.
- The update script is now very simple since it only has to update one mailbox and log to stdout.
- A simple `neomutt` macro can still trigger an immediate update when I want one.

Plus, it was a great way to learn how toi use a few more rust crates!
