// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "fidl/tree_visitor.h"

#include <map>

#include "fidl/raw_ast.h"

namespace fidl {
namespace raw {

void DeclarationOrderTreeVisitor::OnFile(std::unique_ptr<File> const& element) {
  OnSourceElementStart(*element);
  OnLibraryDecl(element->library_decl);

  auto alias_decls_it = element->alias_list.begin();
  auto bits_decls_it = element->bits_declaration_list.begin();
  auto const_decls_it = element->const_declaration_list.begin();
  auto enum_decls_it = element->enum_declaration_list.begin();
  auto protocol_decls_it = element->protocol_declaration_list.begin();
  auto resource_decls_it = element->resource_declaration_list.begin();
  auto service_decls_it = element->service_declaration_list.begin();
  auto struct_decls_it = element->struct_declaration_list.begin();
  auto table_decls_it = element->table_declaration_list.begin();
  auto type_decls_it = element->type_decls.begin();
  auto union_decls_it = element->union_declaration_list.begin();
  auto using_decls_it = element->using_list.begin();

  enum Next {
    alias_t,
    bits_t,
    const_t,
    enum_t,
    protocol_t,
    resource_t,
    service_t,
    struct_t,
    table_t,
    type_decl_t,
    union_t,
    using_t,
  };

  std::map<const char*, Next> m;
  for (;;) {
    // We want to visit these in declaration order, rather than grouped
    // by type of declaration.  std::map is sorted by key.  For each of
    // these lists of declarations, we make a map where the key is "the
    // next start location of the earliest element in the list" to a
    // variable representing the type.  We then identify which type was
    // put earliest in the map.  That will be the earliest declaration
    // in the file.  We then visit the declaration accordingly.
    m.clear();
    if (alias_decls_it != element->alias_list.end()) {
      m[(*alias_decls_it)->start_.previous_end().data().data()] = alias_t;
    }
    if (bits_decls_it != element->bits_declaration_list.end()) {
      m[(*bits_decls_it)->start_.previous_end().data().data()] = bits_t;
    }
    if (const_decls_it != element->const_declaration_list.end()) {
      m[(*const_decls_it)->start_.previous_end().data().data()] = const_t;
    }
    if (enum_decls_it != element->enum_declaration_list.end()) {
      m[(*enum_decls_it)->start_.previous_end().data().data()] = enum_t;
    }
    if (protocol_decls_it != element->protocol_declaration_list.end()) {
      if (*protocol_decls_it == nullptr) {
        // Used to indicate empty, so let's wind it forward.
        protocol_decls_it = element->protocol_declaration_list.end();
      } else {
        m[(*protocol_decls_it)->start_.previous_end().data().data()] = protocol_t;
      }
    }
    if (resource_decls_it != element->resource_declaration_list.end()) {
      m[(*resource_decls_it)->start_.previous_end().data().data()] = resource_t;
    }
    if (service_decls_it != element->service_declaration_list.end()) {
      m[(*service_decls_it)->start_.previous_end().data().data()] = service_t;
    }
    if (struct_decls_it != element->struct_declaration_list.end()) {
      m[(*struct_decls_it)->start_.previous_end().data().data()] = struct_t;
    }
    if (table_decls_it != element->table_declaration_list.end()) {
      m[(*table_decls_it)->start_.previous_end().data().data()] = table_t;
    }
    if (type_decls_it != element->type_decls.end()) {
      m[(*type_decls_it)->start_.previous_end().data().data()] = type_decl_t;
    }
    if (union_decls_it != element->union_declaration_list.end()) {
      m[(*union_decls_it)->start_.previous_end().data().data()] = union_t;
    }
    if (using_decls_it != element->using_list.end()) {
      m[(*using_decls_it)->start_.previous_end().data().data()] = using_t;
    }
    if (m.size() == 0)
      break;

    // And the earliest top level declaration is...
    switch (m.begin()->second) {
      case alias_t:
        OnAliasDeclaration(*alias_decls_it);
        ++alias_decls_it;
        break;
      case bits_t:
        OnBitsDeclaration(*bits_decls_it);
        ++bits_decls_it;
        break;
      case const_t:
        OnConstDeclaration(*const_decls_it);
        ++const_decls_it;
        break;
      case enum_t:
        OnEnumDeclaration(*enum_decls_it);
        ++enum_decls_it;
        break;
      case protocol_t:
        OnProtocolDeclaration(*protocol_decls_it);
        ++protocol_decls_it;
        break;
      case resource_t:
        OnResourceDeclaration(*resource_decls_it);
        ++resource_decls_it;
        break;
      case service_t:
        OnServiceDeclaration(*service_decls_it);
        ++service_decls_it;
        break;
      case struct_t:
        OnStructDeclaration(*struct_decls_it);
        ++struct_decls_it;
        break;
      case table_t:
        OnTableDeclaration(*table_decls_it);
        ++table_decls_it;
        break;
      case type_decl_t:
        OnTypeDecl(*type_decls_it);
        ++type_decls_it;
        break;
      case union_t:
        OnUnionDeclaration(*union_decls_it);
        ++union_decls_it;
        break;
      case using_t:
        OnUsing(*using_decls_it);
        ++using_decls_it;
        break;
    }
  }
  OnSourceElementEnd(*element);
}

void DeclarationOrderTreeVisitor::OnProtocolDeclaration(
    std::unique_ptr<ProtocolDeclaration> const& element) {
  SourceElementMark sem(this, *element);
  if (IsAttributeListDefined(element->attributes)) {
    OnAttributeList(element->attributes);
  }
  OnIdentifier(element->identifier);

  auto compose_it = element->composed_protocols.begin();
  auto methods_it = element->methods.begin();

  enum Next {
    compose_t,
    method_t,
  };

  std::map<const char*, Next> m;
  for (;;) {
    // Sort in declaration order.
    m.clear();
    if (compose_it != element->composed_protocols.end()) {
      m[(*compose_it)->start_.previous_end().data().data()] = compose_t;
    }
    if (methods_it != element->methods.end()) {
      m[(*methods_it)->start_.previous_end().data().data()] = method_t;
    }
    if (m.size() == 0)
      return;

    // And the earliest declaration is...
    switch (m.begin()->second) {
      case compose_t:
        OnComposeProtocol(*compose_it);
        ++compose_it;
        break;
      case method_t:
        OnProtocolMethod(*methods_it);
        ++methods_it;
        break;
    }
  }
}

}  // namespace raw
}  // namespace fidl
