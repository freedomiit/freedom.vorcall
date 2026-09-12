using System;
using Microsoft.EntityFrameworkCore.Migrations;
using Npgsql.EntityFrameworkCore.PostgreSQL.Metadata;

#nullable disable

namespace Vorcall.Server.Migrations
{
    // Rooms become channels. Every public room turns into a category holding a text channel (the
    // room's slug) and a voice channel, every DM into a DM channel, and every message, attachment
    // and read cursor follows through a temporary id map. Hand-ordered so the data moves before the
    // rooms tables go, and one-way because of it: back the database up before applying it.
    /// <inheritdoc />
    public partial class ChannelsRolesProfiles : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            // The new tables first, channels before server so general_channel_id has a target and
            // before channel_reads and channel_overrides so theirs do too.
            migrationBuilder.CreateTable(
                name: "categories",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    position = table.Column<int>(type: "integer", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_categories", x => x.id);
                });

            migrationBuilder.CreateTable(
                name: "channels",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    kind = table.Column<short>(type: "smallint", nullable: false),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    topic = table.Column<string>(type: "character varying(256)", maxLength: 256, nullable: false, defaultValue: ""),
                    category_id = table.Column<long>(type: "bigint", nullable: true),
                    position = table.Column<int>(type: "integer", nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false),
                    dm_low = table.Column<long>(type: "bigint", nullable: true),
                    dm_high = table.Column<long>(type: "bigint", nullable: true)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_channels", x => x.id);
                    table.ForeignKey(
                        name: "FK_channels_categories_category_id",
                        column: x => x.category_id,
                        principalTable: "categories",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            // The channel indexes and the everyone index are created before the data steps below,
            // so the uniqueness they promise holds for the rows this migration writes too.
            migrationBuilder.CreateIndex(
                name: "IX_channels_category_id_position",
                table: "channels",
                columns: new[] { "category_id", "position" });

            migrationBuilder.CreateIndex(
                name: "IX_channels_dm_low_dm_high",
                table: "channels",
                columns: new[] { "dm_low", "dm_high" },
                unique: true,
                filter: "\"kind\" = 3");

            migrationBuilder.CreateTable(
                name: "roles",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    color = table.Column<int>(type: "integer", nullable: true),
                    icon_emoji = table.Column<string>(type: "character varying(16)", maxLength: 16, nullable: false, defaultValue: ""),
                    icon_image_id = table.Column<long>(type: "bigint", nullable: true),
                    position = table.Column<int>(type: "integer", nullable: false),
                    permissions = table.Column<long>(type: "bigint", nullable: false),
                    hoist = table.Column<bool>(type: "boolean", nullable: false),
                    is_everyone = table.Column<bool>(type: "boolean", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_roles", x => x.id);
                });

            migrationBuilder.CreateIndex(
                name: "IX_roles_is_everyone",
                table: "roles",
                column: "is_everyone",
                unique: true,
                filter: "\"is_everyone\"");

            migrationBuilder.CreateTable(
                name: "images",
                columns: table => new
                {
                    id = table.Column<long>(type: "bigint", nullable: false)
                        .Annotation("Npgsql:ValueGenerationStrategy", NpgsqlValueGenerationStrategy.IdentityByDefaultColumn),
                    purpose = table.Column<short>(type: "smallint", nullable: false),
                    uploader_id = table.Column<long>(type: "bigint", nullable: true),
                    content_type = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false),
                    size = table.Column<long>(type: "bigint", nullable: false),
                    created_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_images", x => x.id);
                    table.ForeignKey(
                        name: "FK_images_users_uploader_id",
                        column: x => x.uploader_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateTable(
                name: "bans",
                columns: table => new
                {
                    user_id = table.Column<long>(type: "bigint", nullable: false),
                    banned_by = table.Column<long>(type: "bigint", nullable: true),
                    reason = table.Column<string>(type: "character varying(256)", maxLength: 256, nullable: false),
                    banned_at = table.Column<DateTime>(type: "timestamp with time zone", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_bans", x => x.user_id);
                    table.ForeignKey(
                        name: "FK_bans_users_user_id",
                        column: x => x.user_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateTable(
                name: "server",
                columns: table => new
                {
                    id = table.Column<short>(type: "smallint", nullable: false),
                    name = table.Column<string>(type: "character varying(32)", maxLength: 32, nullable: false, defaultValue: "Vorcall"),
                    description = table.Column<string>(type: "character varying(256)", maxLength: 256, nullable: false, defaultValue: ""),
                    icon_image_id = table.Column<long>(type: "bigint", nullable: true),
                    owner_id = table.Column<long>(type: "bigint", nullable: true),
                    general_channel_id = table.Column<long>(type: "bigint", nullable: true)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_server", x => x.id);
                    table.ForeignKey(
                        name: "FK_server_channels_general_channel_id",
                        column: x => x.general_channel_id,
                        principalTable: "channels",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                    table.ForeignKey(
                        name: "FK_server_users_owner_id",
                        column: x => x.owner_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.SetNull);
                });

            migrationBuilder.CreateTable(
                name: "member_roles",
                columns: table => new
                {
                    user_id = table.Column<long>(type: "bigint", nullable: false),
                    role_id = table.Column<long>(type: "bigint", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_member_roles", x => new { x.user_id, x.role_id });
                    table.ForeignKey(
                        name: "FK_member_roles_roles_role_id",
                        column: x => x.role_id,
                        principalTable: "roles",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                    table.ForeignKey(
                        name: "FK_member_roles_users_user_id",
                        column: x => x.user_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateTable(
                name: "channel_overrides",
                columns: table => new
                {
                    channel_id = table.Column<long>(type: "bigint", nullable: false),
                    target_kind = table.Column<short>(type: "smallint", nullable: false),
                    target_id = table.Column<long>(type: "bigint", nullable: false),
                    allow = table.Column<long>(type: "bigint", nullable: false),
                    deny = table.Column<long>(type: "bigint", nullable: false)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_channel_overrides", x => new { x.channel_id, x.target_kind, x.target_id });
                    table.ForeignKey(
                        name: "FK_channel_overrides_channels_channel_id",
                        column: x => x.channel_id,
                        principalTable: "channels",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.CreateTable(
                name: "channel_reads",
                columns: table => new
                {
                    channel_id = table.Column<long>(type: "bigint", nullable: false),
                    user_id = table.Column<long>(type: "bigint", nullable: false),
                    last_read_message_id = table.Column<long>(type: "bigint", nullable: false, defaultValue: 0L)
                },
                constraints: table =>
                {
                    table.PrimaryKey("PK_channel_reads", x => new { x.channel_id, x.user_id });
                    table.ForeignKey(
                        name: "FK_channel_reads_channels_channel_id",
                        column: x => x.channel_id,
                        principalTable: "channels",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                    table.ForeignKey(
                        name: "FK_channel_reads_users_user_id",
                        column: x => x.user_id,
                        principalTable: "users",
                        principalColumn: "id",
                        onDelete: ReferentialAction.Cascade);
                });

            migrationBuilder.AddColumn<int>(
                name: "accent_color",
                table: "users",
                type: "integer",
                nullable: true);

            migrationBuilder.AddColumn<long>(
                name: "avatar_image_id",
                table: "users",
                type: "bigint",
                nullable: true);

            migrationBuilder.AddColumn<long>(
                name: "banner_image_id",
                table: "users",
                type: "bigint",
                nullable: true);

            migrationBuilder.AddColumn<string>(
                name: "description",
                table: "users",
                type: "character varying(256)",
                maxLength: 256,
                nullable: false,
                defaultValue: "");

            migrationBuilder.AddColumn<string>(
                name: "nickname",
                table: "users",
                type: "character varying(32)",
                maxLength: 32,
                nullable: true);

            migrationBuilder.AddColumn<bool>(
                name: "server_deafened",
                table: "users",
                type: "boolean",
                nullable: false,
                defaultValue: false);

            migrationBuilder.AddColumn<bool>(
                name: "server_muted",
                table: "users",
                type: "boolean",
                nullable: false,
                defaultValue: false);

            migrationBuilder.AddColumn<long>(
                name: "created_by",
                table: "invites",
                type: "bigint",
                nullable: true);

            // The two singleton rows a fresh database gets from Seed instead. The owner is the
            // lowest account id, which "server set-owner" exists to correct; the permissions are
            // Permissions.Perms.EveryoneDefault, spelled out because a migration is a snapshot and
            // must not move when that constant does.
            migrationBuilder.Sql(
                """
                INSERT INTO server (id, name, description, owner_id)
                VALUES (1, 'Vorcall', '', (SELECT MIN(id) FROM users));

                INSERT INTO roles (name, color, icon_emoji, position, permissions, hoist, is_everyone)
                VALUES ('everyone', NULL, '', 0, 1109760, false, true);
                """);

            // The room walk. The map is what carries every message, attachment and cursor over, so
            // it outlives the walk and is dropped only after the last read of it.
            migrationBuilder.Sql(
                """
                CREATE TEMPORARY TABLE room_map (
                    old_id character varying(48) PRIMARY KEY,
                    channel_id bigint NOT NULL
                );

                DO $$
                DECLARE
                    room RECORD;
                    new_category_id bigint;
                    new_channel_id bigint;
                    next_position integer := 0;
                BEGIN
                    -- general first, then oldest first, which is the order the clients have been
                    -- showing the room list in.
                    FOR room IN
                        SELECT id, name, created_at
                        FROM rooms
                        WHERE kind = 1
                        ORDER BY (id <> 'general'), created_at, id
                    LOOP
                        -- general's stored display name is the lowercase slug the migration that
                        -- created it wrote, so its category and voice channel take the casing a
                        -- fresh database gets from Seed ('General') rather than that.
                        INSERT INTO categories (name, position)
                        VALUES (CASE WHEN room.id = 'general' THEN 'General' ELSE room.name END, next_position)
                        RETURNING id INTO new_category_id;

                        -- The text channel keeps the room's slug, which is the name every client,
                        -- mention and habit already has for it.
                        INSERT INTO channels (kind, name, category_id, position, created_at)
                        VALUES (1, room.id, new_category_id, 0, room.created_at)
                        RETURNING id INTO new_channel_id;

                        INSERT INTO room_map (old_id, channel_id) VALUES (room.id, new_channel_id);

                        INSERT INTO channels (kind, name, category_id, position, created_at)
                        VALUES (2, CASE WHEN room.id = 'general' THEN 'General' ELSE room.name END, new_category_id, 1, room.created_at);

                        next_position := next_position + 1;
                    END LOOP;

                    -- A DM's id is dm-<lower user id>-<higher user id>, exactly the pair the
                    -- channel stores, so each half is one part of the id.
                    FOR room IN
                        SELECT id, created_at
                        FROM rooms
                        WHERE kind = 2
                        ORDER BY created_at, id
                    LOOP
                        INSERT INTO channels (kind, name, category_id, position, created_at, dm_low, dm_high)
                        VALUES (
                            3,
                            '',
                            NULL,
                            0,
                            room.created_at,
                            split_part(room.id, '-', 2)::bigint,
                            split_part(room.id, '-', 3)::bigint)
                        RETURNING id INTO new_channel_id;

                        INSERT INTO room_map (old_id, channel_id) VALUES (room.id, new_channel_id);
                    END LOOP;
                END $$;

                UPDATE server
                SET general_channel_id = (SELECT channel_id FROM room_map WHERE old_id = 'general');
                """);

            // Nullable first so the backfill below has somewhere to write; required once it has.
            migrationBuilder.AddColumn<long>(
                name: "channel_id",
                table: "messages",
                type: "bigint",
                nullable: true);

            migrationBuilder.AddColumn<long>(
                name: "channel_id",
                table: "attachments",
                type: "bigint",
                nullable: true);

            migrationBuilder.Sql(
                """
                UPDATE messages m
                SET channel_id = map.channel_id
                FROM room_map map
                WHERE m.room_id = map.old_id;

                UPDATE attachments a
                SET channel_id = map.channel_id
                FROM room_map map
                WHERE a.room_id = map.old_id;

                -- room_id never had a foreign key, so a row may name a room that is no longer
                -- there. general is the one channel that always exists, and keeping such a row
                -- beats failing the upgrade on it. A DM's room_id looks like dm-<low>-<high>, and
                -- such a row is tombstoned rather than re-homed live: general is visible to the
                -- whole server, and a tombstone keeps the row without making its content readable
                -- by everyone. mention_everyone/mention_here are not cleared here because this
                -- point in the migration predates the AddColumn calls that create them (below);
                -- they arrive with a false default, which every pre-existing row gets anyway.
                DO $$
                DECLARE
                    orphaned_count integer;
                    orphaned_dm_count integer;
                BEGIN
                    UPDATE messages
                    SET channel_id = (SELECT channel_id FROM room_map WHERE old_id = 'general')
                    WHERE channel_id IS NULL AND room_id NOT LIKE 'dm-%';
                    GET DIAGNOSTICS orphaned_count = ROW_COUNT;

                    UPDATE attachments
                    SET channel_id = (SELECT channel_id FROM room_map WHERE old_id = 'general')
                    WHERE channel_id IS NULL AND room_id NOT LIKE 'dm-%';

                    UPDATE messages
                    SET channel_id = (SELECT channel_id FROM room_map WHERE old_id = 'general'),
                        deleted_at = now(),
                        text = '',
                        mention_ids = ARRAY[]::bigint[]
                    WHERE channel_id IS NULL AND room_id LIKE 'dm-%';
                    GET DIAGNOSTICS orphaned_dm_count = ROW_COUNT;

                    UPDATE attachments
                    SET channel_id = (SELECT channel_id FROM room_map WHERE old_id = 'general')
                    WHERE channel_id IS NULL AND room_id LIKE 'dm-%';

                    RAISE NOTICE 'channels migration: re-homed % orphaned messages into general', orphaned_count;
                    RAISE NOTICE 'channels migration: tombstoned % orphaned direct-message rows into general', orphaned_dm_count;
                END $$;
                """);

            migrationBuilder.AlterColumn<long>(
                name: "channel_id",
                table: "messages",
                type: "bigint",
                nullable: false,
                oldClrType: typeof(long),
                oldType: "bigint",
                oldNullable: true);

            migrationBuilder.AlterColumn<long>(
                name: "channel_id",
                table: "attachments",
                type: "bigint",
                nullable: false,
                oldClrType: typeof(long),
                oldType: "bigint",
                oldNullable: true);

            migrationBuilder.DropIndex(
                name: "IX_messages_room_id_id",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "room_id",
                table: "messages");

            migrationBuilder.DropColumn(
                name: "room_id",
                table: "attachments");

            migrationBuilder.CreateIndex(
                name: "IX_messages_channel_id_id",
                table: "messages",
                columns: new[] { "channel_id", "id" });

            migrationBuilder.CreateIndex(
                name: "IX_attachments_channel_id",
                table: "attachments",
                column: "channel_id");

            migrationBuilder.AddForeignKey(
                name: "FK_messages_channels_channel_id",
                table: "messages",
                column: "channel_id",
                principalTable: "channels",
                principalColumn: "id",
                onDelete: ReferentialAction.Cascade);

            migrationBuilder.AddForeignKey(
                name: "FK_attachments_channels_channel_id",
                table: "attachments",
                column: "channel_id",
                principalTable: "channels",
                principalColumn: "id",
                onDelete: ReferentialAction.Cascade);

            migrationBuilder.AddColumn<bool>(
                name: "mention_everyone",
                table: "messages",
                type: "boolean",
                nullable: false,
                defaultValue: false);

            migrationBuilder.AddColumn<bool>(
                name: "mention_here",
                table: "messages",
                type: "boolean",
                nullable: false,
                defaultValue: false);

            // The cursor is all that survives a membership: who may see a channel is a permission
            // now. Only existing memberships become rows, and a channel with no row for an account
            // reads as entirely unread, which is what ChannelDirectory does with a missing cursor.
            migrationBuilder.Sql(
                """
                INSERT INTO channel_reads (channel_id, user_id, last_read_message_id)
                SELECT map.channel_id, rm.user_id, rm.last_read_message_id
                FROM room_members rm
                JOIN room_map map ON map.old_id = rm.room_id;
                """);

            migrationBuilder.DropTable(
                name: "room_members");

            migrationBuilder.DropTable(
                name: "rooms");

            migrationBuilder.Sql("DROP TABLE room_map;");

            migrationBuilder.CreateIndex(
                name: "IX_channel_reads_user_id",
                table: "channel_reads",
                column: "user_id");

            migrationBuilder.CreateIndex(
                name: "IX_images_created_at",
                table: "images",
                column: "created_at");

            migrationBuilder.CreateIndex(
                name: "IX_images_uploader_id",
                table: "images",
                column: "uploader_id");

            migrationBuilder.CreateIndex(
                name: "IX_member_roles_role_id",
                table: "member_roles",
                column: "role_id");

            migrationBuilder.CreateIndex(
                name: "IX_server_general_channel_id",
                table: "server",
                column: "general_channel_id");

            migrationBuilder.CreateIndex(
                name: "IX_server_owner_id",
                table: "server",
                column: "owner_id");
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
            => throw new NotSupportedException(
                "The channels migration is one-way: restore the database backup taken before the upgrade.");
    }
}
