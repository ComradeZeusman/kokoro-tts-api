use kokoro_en::admin_auth;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use kokoro_en::account::{AccountStore, SubscriptionDuration};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "kokoro-admin",
    version,
    about = "Kokoro TTS account administration"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Configure the initial CLI administrator.
    SetupAdmin {
        #[arg(long)]
        email: String,
    },

    /// Change the administrator password.
    ChangePassword,

    /// List or inspect customer accounts.
    Accounts {
        #[command(subcommand)]
        command: AccountCommand,
    },

    /// Activate an account and create a subscription.
    Activate(DurationArgs),

    /// Extend an account's current subscription.
    Extend(ExtendArgs),

    /// Suspend an account.
    Suspend { account_id: String },

    /// Reactivate a suspended account.
    Unsuspend { account_id: String },

    /// Manage customer API keys.
    Keys {
        #[command(subcommand)]
        command: KeyCommand,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Create a customer account and issue its initial API key.
    Create {
        #[arg(long)]
        name: String,

        #[arg(long)]
        email: String,
    },

    /// Set or reset a customer's password.
    SetPassword { account_id: String },

    /// List all registered accounts.
    List,

    /// Show account and subscription details.
    Show { account_id: String },
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Revoke existing keys and issue a replacement.
    Rotate { account_id: String },

    /// Revoke all active API keys.
    Revoke { account_id: String },
}

#[derive(Args)]
struct DurationArgs {
    account_id: String,

    #[arg(long, conflicts_with = "months", required_unless_present = "months")]
    days: Option<u32>,

    #[arg(long, conflicts_with = "days", required_unless_present = "days")]
    months: Option<u32>,

    #[arg(long)]
    characters: i64,

    /// Speech-to-text allowance in seconds.
    #[arg(long, default_value_t = 0)]
    stt_seconds: i64,
}

#[derive(Args)]
struct ExtendArgs {
    account_id: String,

    #[arg(long, conflicts_with = "months", required_unless_present = "months")]
    days: Option<u32>,

    #[arg(long, conflicts_with = "days", required_unless_present = "days")]
    months: Option<u32>,

    #[arg(long, default_value_t = 0)]
    add_characters: i64,

    /// Additional speech-to-text allowance in seconds.
    #[arg(long, default_value_t = 0)]
    add_stt_seconds: i64,
}

fn database_path() -> PathBuf {
    std::env::var_os("KOKORO_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/workspace/data/kokoro.db"))
}

fn make_duration(days: Option<u32>, months: Option<u32>) -> Result<SubscriptionDuration> {
    match (days, months) {
        (Some(value), None) if value > 0 => Ok(SubscriptionDuration::Days(value)),
        (None, Some(value)) if value > 0 => Ok(SubscriptionDuration::Months(value)),
        (Some(_), None) | (None, Some(_)) => {
            bail!("Subscription duration must be greater than zero.")
        }
        _ => bail!("Specify exactly one of --days or --months."),
    }
}

fn format_timestamp(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|date| date.to_rfc3339())
        .unwrap_or_else(|| format!("Invalid timestamp ({})", timestamp))
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    if let Command::SetupAdmin { email } = &cli.command {
        return admin_auth::setup(email);
    }

    admin_auth::authenticate()?;

    if matches!(&cli.command, Command::ChangePassword) {
        let store = AccountStore::new(database_path())?;

        // Revoke browser sessions before changing the password.
        // If the password change fails, administrators must log in again.
        let revoked = store.revoke_all_admin_sessions()?;

        admin_auth::change_password()?;

        println!("Revoked {} administrator browser session(s).", revoked);
        return Ok(());
    }

    let store = AccountStore::new(database_path())?;

    match cli.command {
        Command::SetupAdmin { .. } | Command::ChangePassword => {
            unreachable!("Authentication commands return before account administration.")
        }

        Command::Accounts { command } => match command {
            AccountCommand::Create { name, email } => {
                let password = admin_auth::prompt_customer_password()?;
                let password_hash = admin_auth::hash_customer_password(&password)?;

                let registration = store.create_customer_account(&name, &email, &password_hash)?;

                println!("Customer account created successfully.");
                println!("Account ID: {}", registration.account_id);
                println!("Name:       {}", name.trim());
                println!("Email:      {}", email.trim().to_ascii_lowercase());
                println!("Status:     pending");
                println!();
                println!("INITIAL API KEY (shown only once):");
                println!("{}", registration.api_key);
                println!();
                println!("Store this API key securely. It cannot be retrieved later.");
                println!("The customer must change this password at first login.");
            }

            AccountCommand::SetPassword { account_id } => {
                let details = store.get_account(&account_id)?;

                if details.is_none() {
                    bail!("Account not found: {}", account_id);
                }

                let password = admin_auth::prompt_customer_password()?;
                let password_hash = admin_auth::hash_customer_password(&password)?;

                store.set_customer_password(&account_id, &password_hash, true)?;

                println!("Customer password set successfully.");
                println!("Account: {}", account_id);
                println!("The customer must change this password at next login.");
                println!("Existing customer web sessions have been invalidated.");
            }

            AccountCommand::List => {
                let accounts = store.list_accounts()?;

                if accounts.is_empty() {
                    println!("No accounts registered yet.");
                } else {
                    println!(
                        "{:<22} {:<24} {:<30} {:<12} {}",
                        "ACCOUNT ID", "NAME", "EMAIL", "STATUS", "CREATED"
                    );
                    println!("{}", "-".repeat(112));

                    for account in accounts {
                        println!(
                            "{:<22} {:<24} {:<30} {:<12} {}",
                            account.id,
                            account.name,
                            account.email,
                            account.status,
                            format_timestamp(account.created_at)
                        );
                    }
                }
            }

            AccountCommand::Show { account_id } => {
                let details = store.get_account(&account_id)?;

                let Some(details) = details else {
                    bail!("Account not found: {}", account_id);
                };

                let account = details.account;

                println!("ACCOUNT");
                println!("-------");
                println!("ID:       {}", account.id);
                println!("Name:     {}", account.name);
                println!("Email:    {}", account.email);
                println!("Status:   {}", account.status);
                println!("Created:  {}", format_timestamp(account.created_at));
                println!("Active API keys: {}", details.active_api_keys);

                println!();
                println!("SUBSCRIPTION");
                println!("------------");

                if let Some(subscription) = details.subscription {
                    println!("ID:              {}", subscription.id);
                    println!("Status:          {}", subscription.status);
                    println!(
                        "Starts:          {}",
                        format_timestamp(subscription.starts_at)
                    );
                    println!(
                        "Expires:         {}",
                        format_timestamp(subscription.expires_at)
                    );
                    println!("Character limit: {}", subscription.character_limit);
                    println!("Characters used: {}", subscription.characters_used);
                    println!(
                        "Characters left: {}",
                        (subscription.character_limit - subscription.characters_used).max(0)
                    );
                } else {
                    println!("No subscription found.");
                }
            }
        },

        Command::Activate(args) => {
            let duration = make_duration(args.days, args.months)?;

            if args.stt_seconds < 0 {
                bail!("STT seconds cannot be negative.");
            }

            let stt_limit_ms = args
                .stt_seconds
                .checked_mul(1000)
                .ok_or_else(|| anyhow::anyhow!("STT allowance is too large."))?;

            store.activate_with_stt(&args.account_id, duration, args.characters, stt_limit_ms)?;

            println!("Account activated successfully.");
            println!("Account: {}", args.account_id);
            println!("Character limit: {}", args.characters);
            println!("STT allowance: {} seconds", args.stt_seconds);
        }

        Command::Extend(args) => {
            let duration = make_duration(args.days, args.months)?;

            if args.add_stt_seconds < 0 {
                bail!("Additional STT seconds cannot be negative.");
            }

            let additional_stt_ms = args
                .add_stt_seconds
                .checked_mul(1000)
                .ok_or_else(|| anyhow::anyhow!("Additional STT allowance is too large."))?;

            store.extend_subscription_with_stt(
                &args.account_id,
                duration,
                args.add_characters,
                additional_stt_ms,
            )?;

            println!("Subscription extended successfully.");
            println!("Account: {}", args.account_id);
            println!("Additional characters: {}", args.add_characters);
            println!("Additional STT: {} seconds", args.add_stt_seconds);
        }

        Command::Suspend { account_id } => {
            store.set_account_status(&account_id, "suspended")?;
            println!("Account suspended: {}", account_id);
        }

        Command::Unsuspend { account_id } => {
            store.set_account_status(&account_id, "active")?;
            println!("Account reactivated: {}", account_id);
            println!("Note: Reactivation does not create or extend a subscription.");
        }

        Command::Keys { command } => match command {
            KeyCommand::Rotate { account_id } => {
                let new_key = store.rotate_api_key(&account_id)?;

                println!("API key rotated successfully.");
                println!("Account: {}", account_id);
                println!();
                println!("NEW API KEY (shown only once):");
                println!("{}", new_key);
                println!();
                println!("Store this key securely. It cannot be retrieved later.");
            }

            KeyCommand::Revoke { account_id } => {
                let count = store.revoke_api_keys(&account_id)?;
                println!("Revoked {} API key(s) for {}.", count, account_id);
            }
        },
    }

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {:#}", error);
        std::process::exit(1);
    }
}
