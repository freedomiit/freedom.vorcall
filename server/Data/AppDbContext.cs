using Microsoft.EntityFrameworkCore;

namespace Vorcall.Server.Data;

public class AppDbContext(DbContextOptions<AppDbContext> options) : DbContext(options)
{
    public DbSet<Message> Messages => Set<Message>();

    public DbSet<User> Users => Set<User>();

    public DbSet<RefreshToken> RefreshTokens => Set<RefreshToken>();

    public DbSet<Invite> Invites => Set<Invite>();

    public DbSet<Room> Rooms => Set<Room>();

    public DbSet<RoomMember> RoomMembers => Set<RoomMember>();

    public DbSet<Reaction> Reactions => Set<Reaction>();

    public DbSet<Attachment> Attachments => Set<Attachment>();

    protected override void OnModelCreating(ModelBuilder modelBuilder)
    {
        var message = modelBuilder.Entity<Message>();
        message.ToTable("messages");
        message.HasKey(m => m.Id);
        message.Property(m => m.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        message.Property(m => m.Author).HasColumnName("author").HasMaxLength(32).IsRequired();
        message.Property(m => m.Text).HasColumnName("text").HasMaxLength(2000).IsRequired();
        message.Property(m => m.SentAt).HasColumnName("sent_at").HasColumnType("timestamp with time zone").IsRequired();

        // The default backfills every message written before rooms existed into general, so the
        // column can be required without rewriting a single existing row.
        message.Property(m => m.RoomId).HasColumnName("room_id").HasMaxLength(48).IsRequired().HasDefaultValue("general");
        message.Property(m => m.UserId).HasColumnName("user_id");
        message.Property(m => m.EditedAt).HasColumnName("edited_at").HasColumnType("timestamp with time zone");
        message.Property(m => m.DeletedAt).HasColumnName("deleted_at").HasColumnType("timestamp with time zone");
        message.Property(m => m.ReplyToId).HasColumnName("reply_to_id");

        // Never null, so a mention query is a plain = ANY without a null branch; the default is
        // what backfills every message written before mentions existed.
        message.Property(m => m.MentionIds)
            .HasColumnName("mention_ids")
            .HasColumnType("bigint[]")
            .IsRequired()
            .HasDefaultValueSql("ARRAY[]::bigint[]");
        message.HasIndex(m => new { m.RoomId, m.Id });

        // No navigation: a message keeps its author text forever, and deleting the account only
        // detaches the id.
        message.HasOne<User>().WithMany().HasForeignKey(m => m.UserId).OnDelete(DeleteBehavior.SetNull);

        var user = modelBuilder.Entity<User>();
        user.ToTable("users");
        user.HasKey(u => u.Id);
        user.Property(u => u.Id).HasColumnName("id").UseIdentityByDefaultColumn();
        user.Property(u => u.Username).HasColumnName("username").HasMaxLength(32).IsRequired();
        user.Property(u => u.UsernameNormalized).HasColumnName("username_normalized").HasMaxLength(32).IsRequired();
        user.Property(u => u.PasswordHash).HasColumnName("password_hash").HasColumnType("text").IsRequired();
        user.Property(u => u.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        user.Property(u => u.LastClientVersion).HasColumnName("last_client_version").HasMaxLength(64);
        user.Property(u => u.LastClientPlatform).HasColumnName("last_client_platform").HasMaxLength(64);
        user.Property(u => u.LastSeenAt).HasColumnName("last_seen_at").HasColumnType("timestamp with time zone");
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
        invite.Property(i => i.UsedByUserId).HasColumnName("used_by_user_id");
        invite.HasIndex(i => i.CodeHash).IsUnique();

        var room = modelBuilder.Entity<Room>();
        room.ToTable("rooms");
        room.HasKey(r => r.Id);
        room.Property(r => r.Id).HasColumnName("id").HasMaxLength(48);
        room.Property(r => r.Kind).HasColumnName("kind").HasConversion<short>().IsRequired();
        room.Property(r => r.Name).HasColumnName("name").HasMaxLength(32).IsRequired();
        room.Property(r => r.CreatedBy).HasColumnName("created_by");
        room.Property(r => r.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();
        room.HasOne<User>().WithMany().HasForeignKey(r => r.CreatedBy).OnDelete(DeleteBehavior.SetNull);

        var roomMember = modelBuilder.Entity<RoomMember>();
        roomMember.ToTable("room_members");
        roomMember.HasKey(m => new { m.RoomId, m.UserId });
        roomMember.Property(m => m.RoomId).HasColumnName("room_id").HasMaxLength(48);
        roomMember.Property(m => m.UserId).HasColumnName("user_id");
        roomMember.Property(m => m.JoinedAt).HasColumnName("joined_at").HasColumnType("timestamp with time zone").IsRequired();
        roomMember.Property(m => m.LastReadMessageId).HasColumnName("last_read_message_id").IsRequired().HasDefaultValue(0L);
        roomMember.HasIndex(m => m.UserId);

        // Deleting a room or an account takes its memberships with it: neither leaves a row
        // pointing at something that is gone.
        roomMember.HasOne<Room>().WithMany().HasForeignKey(m => m.RoomId).OnDelete(DeleteBehavior.Cascade);
        roomMember.HasOne<User>().WithMany().HasForeignKey(m => m.UserId).OnDelete(DeleteBehavior.Cascade);

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
        attachment.Property(a => a.RoomId).HasColumnName("room_id").HasMaxLength(48).IsRequired();
        attachment.Property(a => a.UploaderId).HasColumnName("uploader_id");
        attachment.Property(a => a.MessageId).HasColumnName("message_id");
        attachment.Property(a => a.FileName).HasColumnName("file_name").HasMaxLength(128).IsRequired();
        attachment.Property(a => a.ContentType).HasColumnName("content_type").HasMaxLength(32).IsRequired();
        attachment.Property(a => a.Size).HasColumnName("size").IsRequired();
        attachment.Property(a => a.CreatedAt).HasColumnName("created_at").HasColumnType("timestamp with time zone").IsRequired();

        // The sweeper reads by created_at, the page reader by message_id.
        attachment.HasIndex(a => a.MessageId);
        attachment.HasIndex(a => a.CreatedAt);
        attachment.HasOne<User>().WithMany().HasForeignKey(a => a.UploaderId).OnDelete(DeleteBehavior.SetNull);

        // Deleting a message clears its attachment rows explicitly; SetNull is only the backstop
        // for a row the service did not reach.
        attachment.HasOne<Message>().WithMany().HasForeignKey(a => a.MessageId).OnDelete(DeleteBehavior.SetNull);
    }
}
