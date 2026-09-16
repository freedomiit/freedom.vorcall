using Microsoft.EntityFrameworkCore;

namespace Vorcall.Server.Data;

public class AppDbContext(DbContextOptions<AppDbContext> options) : DbContext(options)
{
    public DbSet<Message> Messages => Set<Message>();

    public DbSet<User> Users => Set<User>();

    public DbSet<RefreshToken> RefreshTokens => Set<RefreshToken>();

    public DbSet<Invite> Invites => Set<Invite>();

    // Exactly one row, keyed Server.RowId.
    public DbSet<Server> Server => Set<Server>();

    public DbSet<Category> Categories => Set<Category>();

    public DbSet<Channel> Channels => Set<Channel>();

    public DbSet<ChannelRead> ChannelReads => Set<ChannelRead>();

    public DbSet<Role> Roles => Set<Role>();

    public DbSet<MemberRole> MemberRoles => Set<MemberRole>();

    public DbSet<ChannelOverride> ChannelOverrides => Set<ChannelOverride>();

    public DbSet<Ban> Bans => Set<Ban>();

    public DbSet<Image> Images => Set<Image>();

    public DbSet<Reaction> Reactions => Set<Reaction>();

    public DbSet<Attachment> Attachments => Set<Attachment>();

    public DbSet<StreamedFile> StreamedFiles => Set<StreamedFile>();

    public DbSet<Sound> Sounds => Set<Sound>();

    public DbSet<Sticker> Stickers => Set<Sticker>();

    protected override void OnModelCreating(ModelBuilder modelBuilder)
    {
        var message = modelBuilder.Entity<Message>();
        message.ToTable("messages");
        message.HasKey(m => m.Id);
        message.Property(m => m.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        message.Property(m => m.Author).HasColumnName("author").HasMaxLength(32).IsRequired();
        message.Property(m => m.Text).HasColumnName("text").HasMaxLength(2000).IsRequired();
        message.Property(m => m.SentAt).HasColumnName("sent_at").HasColumnType("timestamp with time zone").IsRequired();
        message.Property(m => m.ChannelId).HasColumnName("channel_id").IsRequired();
        message.Property(m => m.UserId).HasColumnName("user_id");
        message.Property(m => m.EditedAt).HasColumnName("edited_at").HasColumnType("timestamp with time zone");
        message.Property(m => m.DeletedAt).HasColumnName("deleted_at").HasColumnType("timestamp with time zone");
        message.Property(m => m.ReplyToId).HasColumnName("reply_to_id");
        message.Property(m => m.MentionEveryone).HasColumnName("mention_everyone").IsRequired().HasDefaultValue(false);
        message.Property(m => m.MentionHere).HasColumnName("mention_here").IsRequired().HasDefaultValue(false);
        message.Property(m => m.StickerId).HasColumnName("sticker_id");
        message.Property(m => m.IsSticker).HasColumnName("sticker").IsRequired().HasDefaultValue(false);

        // Never null, so a mention query is a plain = ANY without a null branch; the default is
        // what backfills every message written before mentions existed.
        message.Property(m => m.MentionIds)
            .HasColumnName("mention_ids")
            .HasColumnType("bigint[]")
            .IsRequired()
            .HasDefaultValueSql("ARRAY[]::bigint[]");
        message.HasIndex(m => new { m.ChannelId, m.Id });

        // No navigation: a message keeps its author text forever, and deleting the account only
        // detaches the id.
        message.HasOne<User>().WithMany().HasForeignKey(m => m.UserId).OnDelete(DeleteBehavior.SetNull);

        // Deleting a channel takes its messages with it.
        message.HasOne<Channel>().WithMany().HasForeignKey(m => m.ChannelId).OnDelete(DeleteBehavior.Cascade);

        // Deleting a sticker leaves its messages in place, still flagged as sticker messages.
        message.HasIndex(m => m.StickerId);
        message.HasOne<Sticker>().WithMany().HasForeignKey(m => m.StickerId).OnDelete(DeleteBehavior.SetNull);

        var user = modelBuilder.Entity<User>();
        user.ToTable("users");
        user.HasKey(u => u.Id);
        user.Property(u => u.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        user.Property(u => u.Username).HasColumnName("username").HasMaxLength(32).IsRequired();
        user.Property(u => u.UsernameNormalized).HasColumnName("username_normalized").HasMaxLength(32).IsRequired();
        user.Property(u => u.PasswordHash).HasColumnName("password_hash").HasColumnType("text").IsRequired();
        user.Property(u => u.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        user.Property(u => u.Nickname).HasColumnName("nickname").HasMaxLength(32);
        user.Property(u => u.AvatarImageId).HasColumnName("avatar_image_id");
        user.Property(u => u.BannerImageId).HasColumnName("banner_image_id");
        user.Property(u => u.Description).HasColumnName("description").HasMaxLength(256).IsRequired().HasDefaultValue("");
        user.Property(u => u.AccentColor).HasColumnName("accent_color");
        user.Property(u => u.ServerMuted).HasColumnName("server_muted").IsRequired().HasDefaultValue(false);
        user.Property(u => u.ServerDeafened).HasColumnName("server_deafened").IsRequired().HasDefaultValue(false);
        user.Property(u => u.LastClientVersion).HasColumnName("last_client_version").HasMaxLength(64);
        user.Property(u => u.LastClientPlatform).HasColumnName("last_client_platform").HasMaxLength(64);
        user.Property(u => u.LastSeenAt).HasColumnName("last_seen_at").HasColumnType("timestamp with time zone");
        user.Property(u => u.DisabledAt).HasColumnName("disabled_at").HasColumnType("timestamp with time zone");
        user.HasIndex(u => u.UsernameNormalized).IsUnique();

        var refreshToken = modelBuilder.Entity<RefreshToken>();
        refreshToken.ToTable("refresh_tokens");
        refreshToken.HasKey(t => t.Id);
        refreshToken.Property(t => t.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        refreshToken.Property(t => t.UserId).HasColumnName("user_id").IsRequired();
        refreshToken.Property(t => t.TokenHash).HasColumnName("token_hash").HasColumnType("character(64)").IsRequired();
        refreshToken.Property(t => t.FamilyId).HasColumnName("family_id").IsRequired();
        refreshToken.Property(t => t.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        refreshToken.Property(t => t.LastUsedAt).HasColumnName("last_used_at").HasColumnType("timestamp with time zone").IsRequired();
        refreshToken.Property(t => t.ExpiresAt).HasColumnName("expires_at").HasColumnType("timestamp with time zone").IsRequired();
        refreshToken.Property(t => t.RevokedAt).HasColumnName("revoked_at").HasColumnType("timestamp with time zone");
        refreshToken.Property(t => t.ReplacedById).HasColumnName("replaced_by_id");
        refreshToken.HasIndex(t => t.TokenHash).IsUnique();
        refreshToken.HasIndex(t => t.UserId);
        refreshToken.HasIndex(t => t.FamilyId);
        refreshToken.HasOne(t => t.User).WithMany().HasForeignKey(t => t.UserId).OnDelete(DeleteBehavior.Cascade);

        var invite = modelBuilder.Entity<Invite>();
        invite.ToTable("invites");
        invite.HasKey(i => i.Id);
        invite.Property(i => i.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        invite.Property(i => i.CodeHash).HasColumnName("code_hash").HasColumnType("character(64)").IsRequired();
        invite.Property(i => i.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        invite.Property(i => i.ExpiresAt).HasColumnName("expires_at").HasColumnType("timestamp with time zone").IsRequired();
        invite.Property(i => i.UsedAt).HasColumnName("used_at").HasColumnType("timestamp with time zone");

        // No foreign key, as with used_by_user_id: both are read back by id only, and an invite
        // outlives the account that minted or consumed it.
        invite.Property(i => i.CreatedBy).HasColumnName("created_by");
        invite.Property(i => i.UsedByUserId).HasColumnName("used_by_user_id");
        invite.Property(i => i.RevokedAt).HasColumnName("revoked_at").HasColumnType("timestamp with time zone");
        invite.HasIndex(i => i.CodeHash).IsUnique();

        var server = modelBuilder.Entity<Server>();
        server.ToTable("server");
        server.HasKey(s => s.Id);

        // The id is the constant Server.RowId, not a sequence: the migration and the seeder insert
        // the one row with that id.
        server.Property(s => s.Id).HasColumnName("id").ValueGeneratedNever();
        server.Property(s => s.Name).HasColumnName("name").HasMaxLength(32).IsRequired().HasDefaultValue("Vorcall");
        server.Property(s => s.Description).HasColumnName("description").HasMaxLength(256).IsRequired().HasDefaultValue("");
        server.Property(s => s.IconImageId).HasColumnName("icon_image_id");
        server.Property(s => s.OwnerId).HasColumnName("owner_id");
        server.Property(s => s.GeneralChannelId).HasColumnName("general_channel_id");

        // Losing the owner's account or the general channel must not take the server row with it;
        // the CLI and the seeder repair the null.
        server.HasOne<User>().WithMany().HasForeignKey(s => s.OwnerId).OnDelete(DeleteBehavior.SetNull);
        server.HasOne<Channel>().WithMany().HasForeignKey(s => s.GeneralChannelId).OnDelete(DeleteBehavior.SetNull);

        var category = modelBuilder.Entity<Category>();
        category.ToTable("categories");
        category.HasKey(c => c.Id);
        category.Property(c => c.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        category.Property(c => c.Name).HasColumnName("name").HasMaxLength(32).IsRequired();
        category.Property(c => c.Position).HasColumnName("position").IsRequired();

        var channel = modelBuilder.Entity<Channel>();
        channel.ToTable("channels");
        channel.HasKey(c => c.Id);
        channel.Property(c => c.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        channel.Property(c => c.Kind).HasColumnName("kind").HasConversion<short>().IsRequired();
        channel.Property(c => c.Name).HasColumnName("name").HasMaxLength(32).IsRequired();
        channel.Property(c => c.Topic).HasColumnName("topic").HasMaxLength(256).IsRequired().HasDefaultValue("");
        channel.Property(c => c.CategoryId).HasColumnName("category_id");
        channel.Property(c => c.Position).HasColumnName("position").IsRequired();
        channel.Property(c => c.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        channel.Property(c => c.DmLow).HasColumnName("dm_low");
        channel.Property(c => c.DmHigh).HasColumnName("dm_high");

        // Filtered on the DM kind so the two null columns every text and voice channel carries stay
        // out of it: one DM per pair of accounts, the pair ordered low id first.
        channel.HasIndex(c => new { c.DmLow, c.DmHigh }).IsUnique().HasFilter("\"kind\" = 3");
        channel.HasIndex(c => new { c.CategoryId, c.Position });

        // Deleting a category leaves its channels uncategorised rather than deleting them.
        channel.HasOne<Category>().WithMany().HasForeignKey(c => c.CategoryId).OnDelete(DeleteBehavior.SetNull);

        var channelRead = modelBuilder.Entity<ChannelRead>();
        channelRead.ToTable("channel_reads");
        channelRead.HasKey(r => new { r.ChannelId, r.UserId });
        channelRead.Property(r => r.ChannelId).HasColumnName("channel_id");
        channelRead.Property(r => r.UserId).HasColumnName("user_id");
        channelRead.Property(r => r.LastReadMessageId).HasColumnName("last_read_message_id").IsRequired().HasDefaultValue(0L);
        channelRead.HasIndex(r => r.UserId);

        // Deleting a channel or an account takes its cursors with it: neither leaves a row pointing
        // at something that is gone.
        channelRead.HasOne<Channel>().WithMany().HasForeignKey(r => r.ChannelId).OnDelete(DeleteBehavior.Cascade);
        channelRead.HasOne<User>().WithMany().HasForeignKey(r => r.UserId).OnDelete(DeleteBehavior.Cascade);

        var role = modelBuilder.Entity<Role>();
        role.ToTable("roles");
        role.HasKey(r => r.Id);
        role.Property(r => r.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        role.Property(r => r.Name).HasColumnName("name").HasMaxLength(32).IsRequired();
        role.Property(r => r.Color).HasColumnName("color");
        role.Property(r => r.IconEmoji).HasColumnName("icon_emoji").HasMaxLength(16).IsRequired().HasDefaultValue("");
        role.Property(r => r.IconImageId).HasColumnName("icon_image_id");
        role.Property(r => r.Position).HasColumnName("position").IsRequired();
        role.Property(r => r.Permissions).HasColumnName("permissions").IsRequired();
        role.Property(r => r.Hoist).HasColumnName("hoist").IsRequired();
        role.Property(r => r.IsEveryone).HasColumnName("is_everyone").IsRequired();

        // Filtered so only the true row is constrained: the everyone role cannot be duplicated, and
        // every other role stays out of the index.
        role.HasIndex(r => r.IsEveryone).IsUnique().HasFilter("\"is_everyone\"");

        var memberRole = modelBuilder.Entity<MemberRole>();
        memberRole.ToTable("member_roles");
        memberRole.HasKey(m => new { m.UserId, m.RoleId });
        memberRole.Property(m => m.UserId).HasColumnName("user_id");
        memberRole.Property(m => m.RoleId).HasColumnName("role_id");
        memberRole.HasIndex(m => m.RoleId);
        memberRole.HasOne<User>().WithMany().HasForeignKey(m => m.UserId).OnDelete(DeleteBehavior.Cascade);
        memberRole.HasOne<Role>().WithMany().HasForeignKey(m => m.RoleId).OnDelete(DeleteBehavior.Cascade);

        var channelOverride = modelBuilder.Entity<ChannelOverride>();
        channelOverride.ToTable("channel_overrides");
        channelOverride.HasKey(o => new { o.ChannelId, o.TargetKind, o.TargetId });
        channelOverride.Property(o => o.ChannelId).HasColumnName("channel_id");
        channelOverride.Property(o => o.TargetKind).HasColumnName("target_kind").HasConversion<short>();
        channelOverride.Property(o => o.TargetId).HasColumnName("target_id");
        channelOverride.Property(o => o.Allow).HasColumnName("allow").IsRequired();
        channelOverride.Property(o => o.Deny).HasColumnName("deny").IsRequired();

        // No foreign key on the target: it is a role id or a user id depending on target_kind, and
        // the handlers drop the override when either is deleted.
        channelOverride.HasOne<Channel>().WithMany().HasForeignKey(o => o.ChannelId).OnDelete(DeleteBehavior.Cascade);

        var ban = modelBuilder.Entity<Ban>();
        ban.ToTable("bans");
        ban.HasKey(b => b.UserId);

        // The key is the banned account's id, not a sequence of its own.
        ban.Property(b => b.UserId).HasColumnName("user_id").ValueGeneratedNever();
        ban.Property(b => b.BannedBy).HasColumnName("banned_by");
        ban.Property(b => b.Reason).HasColumnName("reason").HasMaxLength(256).IsRequired();
        ban.Property(b => b.BannedAt).HasColumnName("banned_at").HasColumnType("timestamp with time zone").IsRequired();
        ban.HasOne<User>().WithMany().HasForeignKey(b => b.UserId).OnDelete(DeleteBehavior.Cascade);

        var image = modelBuilder.Entity<Image>();
        image.ToTable("images");
        image.HasKey(i => i.Id);
        image.Property(i => i.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        image.Property(i => i.Purpose).HasColumnName("purpose").HasConversion<short>().IsRequired();
        image.Property(i => i.UploaderId).HasColumnName("uploader_id");
        image.Property(i => i.ContentType).HasColumnName("content_type").HasMaxLength(32).IsRequired();
        image.Property(i => i.Size).HasColumnName("size").IsRequired();
        image.Property(i => i.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();

        // The sweeper reads by created_at.
        image.HasIndex(i => i.CreatedAt);
        image.HasOne<User>().WithMany().HasForeignKey(i => i.UploaderId).OnDelete(DeleteBehavior.SetNull);

        var sound = modelBuilder.Entity<Sound>();
        sound.ToTable("sounds");
        sound.HasKey(s => s.Id);
        sound.Property(s => s.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        sound.Property(s => s.Name).HasColumnName("name").HasMaxLength(32).IsRequired();
        sound.Property(s => s.UploaderId).HasColumnName("uploader_id");
        sound.Property(s => s.ContentType).HasColumnName("content_type").HasMaxLength(64).IsRequired();
        sound.Property(s => s.Size).HasColumnName("size").IsRequired();
        sound.Property(s => s.DurationMs).HasColumnName("duration_ms").IsRequired();
        sound.Property(s => s.Complete).HasColumnName("complete").IsRequired();
        sound.Property(s => s.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();

        // The sweeper reads by created_at.
        sound.HasIndex(s => s.CreatedAt);
        sound.HasOne<User>().WithMany().HasForeignKey(s => s.UploaderId).OnDelete(DeleteBehavior.SetNull);

        var sticker = modelBuilder.Entity<Sticker>();
        sticker.ToTable("stickers");
        sticker.HasKey(s => s.Id);
        sticker.Property(s => s.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        sticker.Property(s => s.Name).HasColumnName("name").HasMaxLength(32).IsRequired();
        sticker.Property(s => s.UploaderId).HasColumnName("uploader_id");
        sticker.Property(s => s.ContentType).HasColumnName("content_type").HasMaxLength(32).IsRequired();
        sticker.Property(s => s.Size).HasColumnName("size").IsRequired();
        sticker.Property(s => s.Complete).HasColumnName("complete").IsRequired();
        sticker.Property(s => s.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();

        // The sweeper reads by created_at.
        sticker.HasIndex(s => s.CreatedAt);
        sticker.HasOne<User>().WithMany().HasForeignKey(s => s.UploaderId).OnDelete(DeleteBehavior.SetNull);

        var reaction = modelBuilder.Entity<Reaction>();
        reaction.ToTable("reactions");
        reaction.HasKey(r => new { r.MessageId, r.UserId, r.Emoji });
        reaction.Property(r => r.MessageId).HasColumnName("message_id");
        reaction.Property(r => r.UserId).HasColumnName("user_id");
        reaction.Property(r => r.Emoji).HasColumnName("emoji").HasMaxLength(16);
        reaction.Property(r => r.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        reaction.HasOne<Message>().WithMany().HasForeignKey(r => r.MessageId).OnDelete(DeleteBehavior.Cascade);
        reaction.HasOne<User>().WithMany().HasForeignKey(r => r.UserId).OnDelete(DeleteBehavior.Cascade);

        var attachment = modelBuilder.Entity<Attachment>();
        attachment.ToTable("attachments");
        attachment.HasKey(a => a.Id);
        attachment.Property(a => a.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        attachment.Property(a => a.ChannelId).HasColumnName("channel_id").IsRequired();
        attachment.Property(a => a.UploaderId).HasColumnName("uploader_id");
        attachment.Property(a => a.MessageId).HasColumnName("message_id");
        attachment.Property(a => a.FileName).HasColumnName("file_name").HasMaxLength(255).IsRequired();
        attachment.Property(a => a.ContentType).HasColumnName("content_type").HasMaxLength(128).IsRequired();
        attachment.Property(a => a.Size).HasColumnName("size").IsRequired();
        attachment.Property(a => a.Complete).HasColumnName("complete").IsRequired();
        attachment.Property(a => a.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();

        // The sweeper reads by created_at, the page reader by message_id.
        attachment.HasIndex(a => a.MessageId);
        attachment.HasIndex(a => a.CreatedAt);
        attachment.HasOne<User>().WithMany().HasForeignKey(a => a.UploaderId).OnDelete(DeleteBehavior.SetNull);
        attachment.HasOne<Channel>().WithMany().HasForeignKey(a => a.ChannelId).OnDelete(DeleteBehavior.Cascade);

        // Deleting a message clears its attachment rows explicitly; SetNull is only the backstop
        // for a row the service did not reach.
        attachment.HasOne<Message>().WithMany().HasForeignKey(a => a.MessageId).OnDelete(DeleteBehavior.SetNull);

        var streamed = modelBuilder.Entity<StreamedFile>();
        streamed.ToTable("streamed_files");
        streamed.HasKey(s => s.Id);
        streamed.Property(s => s.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        streamed.Property(s => s.ChannelId).HasColumnName("channel_id").IsRequired();
        streamed.Property(s => s.OwnerId).HasColumnName("owner_id");
        streamed.Property(s => s.MessageId).HasColumnName("message_id");
        streamed.Property(s => s.FileName).HasColumnName("file_name").HasMaxLength(255).IsRequired();
        streamed.Property(s => s.ContentType).HasColumnName("content_type").HasMaxLength(128).IsRequired();
        streamed.Property(s => s.Size).HasColumnName("size").IsRequired();
        streamed.Property(s => s.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();

        // Read like an attachment's: the sweeper by created_at, the page reader by message_id.
        streamed.HasIndex(s => s.MessageId);
        streamed.HasIndex(s => s.CreatedAt);
        streamed.HasIndex(s => s.ChannelId);
        streamed.HasOne<User>().WithMany().HasForeignKey(s => s.OwnerId).OnDelete(DeleteBehavior.SetNull);
        streamed.HasOne<Channel>().WithMany().HasForeignKey(s => s.ChannelId).OnDelete(DeleteBehavior.Cascade);

        // Deleting a message clears its streamed-file rows explicitly; SetNull is only the
        // backstop for a row the service did not reach.
        streamed.HasOne<Message>().WithMany().HasForeignKey(s => s.MessageId).OnDelete(DeleteBehavior.SetNull);
    }
}
